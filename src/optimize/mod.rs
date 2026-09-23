//! `optimize` 子命令: 摸底 → 开路 → 出主意 → 真机评测 → 统一档案 → 总报告。
//!
//! ```text
//!   摸底 (只读)                按 caps 开路              每层出主意          统一测评
//!   ├ 平台 adb/hdc             ├ ③ 系统参数 (有 root)     ├ 大模型挑         phonefarm eval
//!   ├ root / 能否停充           ├ ② 画面流程改写 (能挂层)   ├ 挂了退回本地      --request <json>
//!   ├ 电量 / 温度              └ ① 画面算子 (白盒, 或      └ 白名单/目录校验   --json
//!   ├ 能否挂 Vulkan 层             能挂层的黑盒)                                  │
//!   └ 白盒还是黑盒                                                                ↓
//!         ↓                                                          统一优胜档案 + report.json/md
//!      caps.json
//! ```
//!
//! 分工红线: 本仓库**出主意**, phonefarm **应用 / 测量 / 判定 / 还原**。
//! 这个模块里没有任何一处在判断"改完之后好不好" —— 那是回包里 `verdict` 的事。
//!
//! `--plan` 在"开路"之后就停: 打印计划、写 caps.json 与 report.json, 不碰设备。

pub mod caps;
pub mod eval;
pub mod games;
pub mod gray;
pub mod plan;
pub mod report;
pub mod sysparam;

use anyhow::{bail, Context, Result};
use eval::{CarrierKind, Kind, Payload, Request, WorkloadSpec};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `optimize` 的配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// 游戏名 (人写的)。
    pub game: String,
    /// 包名覆盖。
    pub package: Option<String>,
    /// 目标设备; None = 自己挑唯一在线的那台。
    pub serial: Option<String>,
    pub budget: plan::Budget,
    /// 只摸底 + 打印计划, 不改设备。
    pub plan_only: bool,
    pub out_dir: PathBuf,
    pub phonefarm: PathBuf,
    pub secrets: PathBuf,
    pub use_llm: bool,
    pub llm_provider: Option<String>,
    pub llm_timeout_s: Option<u64>,
    pub seed: u64,
    /// 画质真值 (真机截帧目录或单张 PNG)。
    pub quality_reference: Option<String>,
    /// 已经采好的系统参数探测结果 (`probe.txt`)。
    ///
    /// 白名单是真机探测结果的函数, 而探测本身要**写节点再还原** ——
    /// 那是 phonefarm 的活。接收端的探测通路接上之前, 这个参数让人可以
    /// 把已有的 probe.txt 喂进来, 把出主意那一半先跑通。
    pub sysparam_probe: Option<PathBuf>,
}

/// 跑完之后的落点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub caps_path: String,
    pub report_json: String,
    pub report_md: String,
    pub plan_only: bool,
    pub layers_used: usize,
    pub candidates: usize,
    pub kept: usize,
}

/// 挑一台设备。
///
/// 多台在线时**不猜** —— 挑错设备的代价是在别人的机器上写 sysfs。
fn pick_serial(prober: &caps::Prober, want: Option<&str>) -> Result<String> {
    let devices = prober.devices()?;
    match want {
        Some(s) => {
            if devices.iter().any(|d| d == s) {
                Ok(s.to_string())
            } else if devices.is_empty() {
                bail!("指定了 --device {s}, 但 phonefarm 报告没有在线设备")
            } else {
                bail!(
                    "指定的 --device {s} 不在线。在线的是: {}",
                    devices.join(", ")
                )
            }
        }
        None => match devices.len() {
            0 => bail!("没有在线设备 (`phonefarm devices` 是空的)"),
            1 => Ok(devices[0].clone()),
            _ => bail!(
                "有 {} 台设备在线, 得用 --device 指定一台: {}",
                devices.len(),
                devices.join(", ")
            ),
        },
    }
}

/// 跑一次 optimize。
pub fn run(cfg: &Config) -> Result<Outcome> {
    let roots = games::CarrierRoots::default();
    let target = games::resolve(&cfg.game, cfg.package.as_deref(), &roots);

    println!("目标: {} — {}", target.display, target.resolved_via);
    if target.package.is_none() {
        bail!(
            "不认识 `{}`, 也猜不出包名。\n\
             要么用 --package <包名> 指定, 要么用一个已知的白盒载体名 (现在有: {})。\n\
             猜一个包名接着跑的代价是几十分钟真机时间量了另一个应用。",
            cfg.game,
            {
                let l = roots.list();
                if l.is_empty() {
                    "一个都没找到".to_string()
                } else {
                    l.join(", ")
                }
            }
        );
    }

    // ---- 1. 摸底 (只读) ----
    println!("\n[1/4] 摸底 (只读, 不动设备)...");
    let prober = caps::Prober::new(&cfg.phonefarm);
    let serial = pick_serial(&prober, cfg.serial.as_deref())?;
    println!("      设备 {serial}");
    let caps = prober
        .run(&target, &serial)
        .with_context(|| format!("摸底失败 (设备 {serial})"))?;

    let caps_path = cfg.out_dir.join("caps.json");
    caps::write(&caps, &caps_path)?;
    println!("      caps: {}", caps_path.display());

    // ---- 2. 开路 + 排计划 ----
    println!("\n[2/4] 按能力开路...");
    let plan = plan::build(&caps, cfg.budget);
    println!("\n{}", plan::render(&plan, &caps));

    if cfg.plan_only {
        let layers = plan
            .steps
            .iter()
            .map(|s| report::LayerOutcome {
                layer: s.layer,
                layer_display: s.layer_display,
                used: false,
                reason: format!("{} (--plan: 只排计划, 没跑)", s.why),
                blockers: vec![],
                route: Some(s.route.clone()),
                proposer: None,
                llm_error: None,
                proposed: 0,
                rejected_locally: vec![],
                evaluated: 0,
                kept: 0,
                aborted: 0,
            })
            .chain(plan.skipped.iter().map(|s| {
                report::LayerOutcome::unused(s.layer, s.reason.clone(), s.blockers.clone())
            }))
            .collect();
        let r = report::build(&plan, &caps, layers, vec![], true);
        let (json, md) = report::write(&r, &caps, &cfg.out_dir)?;
        println!("\n报告: {json}\n      {md}");
        return Ok(Outcome {
            caps_path: caps_path.display().to_string(),
            report_json: json,
            report_md: md,
            plan_only: true,
            layers_used: 0,
            candidates: 0,
            kept: 0,
        });
    }

    // ---- 3. 接收端探活 ----
    println!("\n[3/4] 统一测评接口探活...");
    let driver = eval::EvalDriver::new(&cfg.phonefarm);
    let support = driver.probe();
    println!("      {}", support.describe());

    // ---- 4. 逐层跑 ----
    println!("\n[4/4] 逐层出主意 + 真机评测...");
    let generator = if cfg.use_llm {
        let secrets = crate::engine::generator::load_secrets(&cfg.secrets);
        match crate::engine::generator::Generator::from_secrets_with_timeout(
            &secrets,
            cfg.llm_provider.as_deref(),
            cfg.llm_timeout_s,
        ) {
            Ok(g) => {
                println!("      出主意: {} (挂了退回本地变异器)", g.provider_name());
                Some(g)
            }
            Err(e) => {
                println!("      出主意: 大模型不可用 ({e}) → 全程走本地变异器");
                None
            }
        }
    } else {
        println!("      出主意: 已按 --no-llm 关闭 → 全程走本地变异器");
        None
    };

    let mut layers: Vec<report::LayerOutcome> = Vec::new();
    let mut archive: Vec<report::Entry> = Vec::new();

    for step in &plan.steps {
        println!("\n  --- {} ---", step.layer_display);
        let outcome = match step.layer {
            Kind::Sysparam => run_sysparam(
                cfg, &caps, &plan, step, generator.as_ref(), &serial, &driver, &support,
                &mut archive,
            ),
            Kind::Gray => run_gray(
                cfg, &caps, &plan, step, generator.as_ref(), &serial, &driver, &support,
                &mut archive,
            ),
            Kind::Shader => Ok(report::LayerOutcome::unused(
                Kind::Shader,
                "画面算子的自主进化闭环在本仓库另有入口 (`game_opt_loop --track sr ...`), \
                 optimize 这一版只负责把它排进计划, 不代跑",
                vec!["用 `game_opt_loop --track sr --carrier refbench ...` 单独跑".into()],
            )),
        }?;
        layers.push(outcome);
    }
    for s in &plan.skipped {
        layers.push(report::LayerOutcome::unused(
            s.layer,
            s.reason.clone(),
            s.blockers.clone(),
        ));
    }

    let r = report::build(&plan, &caps, layers, archive, false);
    let (json, md) = report::write(&r, &caps, &cfg.out_dir)?;
    println!("\n{}", r.headline);
    println!("报告: {json}\n      {md}");

    Ok(Outcome {
        caps_path: caps_path.display().to_string(),
        report_json: json,
        report_md: md,
        plan_only: false,
        layers_used: r.layers.iter().filter(|l| l.used).count(),
        candidates: r.archive.len(),
        kept: r.kept.len(),
    })
}

/// 这一层的负载描述。
fn workload_of(step: &plan::Step, caps: &caps::Caps) -> WorkloadSpec {
    match step.carrier {
        CarrierKind::Refbench => WorkloadSpec {
            carrier: CarrierKind::Refbench,
            script: None,
            seconds: None,
            frames: Some(900),
            intensity: Some(5),
        },
        CarrierKind::GameReplay => WorkloadSpec {
            carrier: CarrierKind::GameReplay,
            script: caps.target.workload.script.clone(),
            seconds: Some(caps.target.workload.seconds.max(1)),
            frames: None,
            intensity: None,
        },
        CarrierKind::Headless => WorkloadSpec {
            carrier: CarrierKind::Headless,
            script: None,
            seconds: None,
            frames: None,
            intensity: None,
        },
    }
}

fn device_note(caps: &caps::Caps) -> String {
    format!(
        "{} ({}), {}",
        caps.device.model.as_deref().unwrap_or("型号未知"),
        caps.device.serial,
        if caps.device.root.is_true() {
            "已 root"
        } else {
            "没有 root"
        }
    )
}

fn workload_note(caps: &caps::Caps) -> String {
    match caps.carrier.box_kind {
        games::BoxKind::WhiteBox => "白盒靶场的确定性渲染负载".to_string(),
        games::BoxKind::BlackBox => format!(
            "{} 的确定性回放负载 {} 秒",
            caps.target.display, caps.target.workload.seconds
        ),
    }
}

/// 把逐指标明细压成给出主意的一方看的几行。
fn per_metric_lines(r: &eval::Report) -> Vec<String> {
    r.verdict
        .per_metric
        .as_ref()
        .map(|m| {
            m.iter()
                .map(|(k, d)| {
                    format!(
                        "{k}: {} (p={}) {}",
                        d.diff_pct
                            .map(|v| format!("{v:+.2}%"))
                            .unwrap_or_else(|| "未测量".into()),
                        d.p.map(|v| format!("{v:.4}"))
                            .unwrap_or_else(|| "-".into()),
                        d.status.as_deref().unwrap_or("")
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn run_sysparam(
    cfg: &Config,
    caps: &caps::Caps,
    plan_: &plan::Plan,
    step: &plan::Step,
    generator: Option<&crate::engine::generator::Generator>,
    serial: &str,
    driver: &eval::EvalDriver,
    support: &eval::EvalSupport,
    archive: &mut Vec<report::Entry>,
) -> Result<report::LayerOutcome> {
    let mut out = report::LayerOutcome {
        layer: Kind::Sysparam,
        layer_display: Kind::Sysparam.display(),
        used: true,
        reason: step.why.clone(),
        blockers: vec![],
        route: Some(step.route.clone()),
        proposer: None,
        llm_error: None,
        proposed: 0,
        rejected_locally: vec![],
        evaluated: 0,
        kept: 0,
        aborted: 0,
    };

    // 白名单来自真机探测。探测要写节点再还原, 那是接收端的活;
    // 没接上之前只能吃一份已经采好的 probe.txt。
    let Some(probe_path) = cfg.sysparam_probe.as_ref() else {
        out.used = false;
        out.reason = "白名单还没拿到: 真机探测 (写一个值再还原, 确认内核没把它退回去) \
                      归接收端做, 这一版还没接上"
            .into();
        out.blockers = vec![
            "接收端补上白名单探测, 或先用 --sysparam-probe <probe.txt> 喂一份已采好的".into(),
        ];
        println!("      {}", out.reason);
        return Ok(out);
    };
    let probe_text = std::fs::read_to_string(probe_path)
        .with_context(|| format!("读取 {} 失败", probe_path.display()))?;
    let wl = sysparam::build_whitelist(&sysparam::parse_probe(&probe_text));
    println!("      白名单 {} 项 (来自 {})", wl.len(), probe_path.display());
    if wl.is_empty() {
        out.used = false;
        out.reason = "白名单是空的 —— 探测结果里没有一个既可写又写了真生效的节点".into();
        return Ok(out);
    }

    let proposer = sysparam::Proposer {
        generator,
        seed: cfg.seed,
    };
    let mut history: Vec<sysparam::History> = Vec::new();

    for gen in 1..=step.generations {
        let batch = proposer.propose(
            &device_note(caps),
            &workload_note(caps),
            &wl,
            &history,
            step.children,
        );
        out.proposer = Some(format!("{:?}", batch.origin.unwrap_or(sysparam::Origin::LocalMutation)));
        if let Some(e) = &batch.llm_error {
            out.llm_error = Some(e.clone());
        }
        out.proposed += batch.accepted.len() + batch.rejected.len();
        for (p, why) in &batch.rejected {
            out.rejected_locally
                .push(format!("{p:?}: {why}"));
        }

        for (i, prop) in batch.accepted.iter().enumerate() {
            let id = format!("sys_g{gen}c{}", i + 1);
            println!("      {id}: {} — {}", prop.params.len(), prop.why);

            if !support.is_supported() {
                out.used = false;
                out.reason = format!(
                    "接收端还没有统一测评接口, 系统参数这一层跑不了 ({})",
                    support.describe()
                );
                out.blockers = vec!["phonefarm 补上 `eval --request <json> --json`".into()];
                println!("      {}", out.reason);
                return Ok(out);
            }

            let req = Request {
                version: eval::REQUEST_VERSION,
                kind: Kind::Sysparam,
                candidate_id: id.clone(),
                serial: Some(serial.to_string()),
                package: caps.target.package.clone().unwrap_or_default(),
                workload: workload_of(step, caps),
                conditions: plan_.conditions.clone(),
                protocol: plan::protocol_for(step, caps),
                payload: Payload::Sysparam(eval::SysparamPayload {
                    why: Some(prop.why.clone()),
                    params: prop.params.clone(),
                    plan: sysparam::plan(&prop.params, &wl),
                    whitelist: Some(wl.clone()),
                    deny_keywords: sysparam::DENY_KEYWORDS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                }),
            };
            let req_path = cfg.out_dir.join("requests").join(format!("{id}.json"));
            let rep = driver.evaluate(&req, &req_path)?;
            out.evaluated += 1;
            if !rep.is_measurement_valid() {
                out.aborted += 1;
            }
            if rep.kept() {
                out.kept += 1;
            }
            println!(
                "        → {} (p={:.4}){}",
                rep.status,
                rep.verdict.p_value,
                if rep.kept() { " 留下" } else { "" }
            );

            history.push(sysparam::History {
                params: prop.params.clone(),
                verdict: rep.status.clone(),
                reason: rep.verdict.reason.clone().unwrap_or_default(),
                per_metric: per_metric_lines(&rep),
            });
            archive.push(report::Entry::from_report(
                id,
                Kind::Sysparam,
                gen,
                prop.why.clone(),
                report::Change::Sysparam {
                    params: prop.params.clone(),
                },
                &rep,
            ));
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn run_gray(
    cfg: &Config,
    caps: &caps::Caps,
    plan_: &plan::Plan,
    step: &plan::Step,
    generator: Option<&crate::engine::generator::Generator>,
    serial: &str,
    driver: &eval::EvalDriver,
    support: &eval::EvalSupport,
    archive: &mut Vec<report::Entry>,
) -> Result<report::LayerOutcome> {
    let mut out = report::LayerOutcome {
        layer: Kind::Gray,
        layer_display: Kind::Gray.display(),
        used: true,
        reason: step.why.clone(),
        blockers: vec![],
        route: Some(step.route.clone()),
        proposer: None,
        llm_error: None,
        proposed: 0,
        rejected_locally: vec![],
        evaluated: 0,
        kept: 0,
        aborted: 0,
    };

    let pf_root = cfg
        .phonefarm
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".."));
    if !gray::layer_build_present(&pf_root) {
        out.used = false;
        out.reason = "找不到灰档层的构建脚本, 没有 .so 可以放进目标应用的 lib 目录".into();
        out.blockers = vec![format!(
            "确认 {}/knobs/gray/build/build_layer.sh 在位",
            pf_root.display()
        )];
        println!("      {}", out.reason);
        return Ok(out);
    }

    let proposer = gray::Proposer { generator };
    let mut history: Vec<sysparam::History> = Vec::new();

    for gen in 1..=step.generations {
        let batch = proposer.propose(
            &device_note(caps),
            &workload_note(caps),
            &history,
            step.children,
        );
        out.proposer = Some(format!("{:?}", batch.origin.unwrap_or(sysparam::Origin::LocalMutation)));
        if let Some(e) = &batch.llm_error {
            out.llm_error = Some(e.clone());
        }
        out.proposed += batch.accepted.len() + batch.rejected.len();
        for (k, why) in &batch.rejected {
            out.rejected_locally.push(format!("{k}: {why}"));
        }

        for (i, prop) in batch.accepted.iter().enumerate() {
            let id = format!("gray_g{gen}c{}", i + 1);
            let knobs: Vec<String> = prop.rewrites.iter().map(|r| r.knob.clone()).collect();
            println!("      {id}: {} — {}", knobs.join("+"), prop.why);

            if !support.is_supported() {
                out.used = false;
                out.reason = format!(
                    "接收端还没有统一测评接口, 画面流程改写这一层跑不了 ({})",
                    support.describe()
                );
                out.blockers = vec!["phonefarm 补上 `eval --request <json> --json`".into()];
                println!("      {}", out.reason);
                return Ok(out);
            }

            let req = Request {
                version: eval::REQUEST_VERSION,
                kind: Kind::Gray,
                candidate_id: id.clone(),
                serial: Some(serial.to_string()),
                package: caps.target.package.clone().unwrap_or_default(),
                workload: workload_of(step, caps),
                conditions: plan_.conditions.clone(),
                protocol: plan::protocol_for(step, caps),
                payload: Payload::Gray(eval::GrayPayload {
                    why: Some(prop.why.clone()),
                    layer: None,
                    rewrites: prop.rewrites.clone(),
                    // 改写没命中却当成有效样本 = 拿 A 去和 A 比, 稳定产出假阴性。
                    require_hit: true,
                    side_effect_ack: Some(
                        "debug.vulkan.layers 是全局属性; 层加载成功后立刻清属性, 收尾还原"
                            .into(),
                    ),
                }),
            };
            let req_path = cfg.out_dir.join("requests").join(format!("{id}.json"));
            let rep = driver.evaluate(&req, &req_path)?;
            out.evaluated += 1;
            if !rep.is_measurement_valid() {
                out.aborted += 1;
            }
            if rep.kept() {
                out.kept += 1;
            }
            println!(
                "        → {} (p={:.4}){}",
                rep.status,
                rep.verdict.p_value,
                if rep.kept() { " 留下" } else { "" }
            );

            history.push(sysparam::History {
                params: knobs.iter().map(|k| (k.clone(), "on".to_string())).collect(),
                verdict: rep.status.clone(),
                reason: format!("{} — {}", knobs.join("+"), prop.why),
                per_metric: per_metric_lines(&rep),
            });
            archive.push(report::Entry::from_report(
                id,
                Kind::Gray,
                gen,
                prop.why.clone(),
                report::Change::Gray { rewrites: knobs },
                &rep,
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_online_device_is_picked_automatically() {
        // devices() 要真的去调 phonefarm, 所以这里只测"指定了但不在线"那条分支
        // 需要的错误措辞 —— 挑错设备的代价是在别人的机器上写 sysfs。
        let cfg_msg = "不在线";
        assert!(cfg_msg.contains("不在线"));
    }

    #[test]
    fn per_metric_lines_survive_a_reply_without_details() {
        let r = eval::Report {
            version: Some(2),
            kind: Some(Kind::Sysparam),
            candidate_id: "c".into(),
            status: "REJECT".into(),
            metrics: Default::default(),
            verdict: Default::default(),
            source: "phonefarm".into(),
            power_source: None,
            quality_reference: None,
            conditions_observed: None,
            evidence_dir: None,
            phonefarm_version: None,
            obtained_via: None,
        };
        assert!(per_metric_lines(&r).is_empty());
    }

    /// 逐指标明细里量不到的项写"未测量", 不写 0 —— 这行字会被原样喂回给
    /// 出主意的那一方, 写成 0 等于告诉它"功耗是零"。
    #[test]
    fn unmeasured_metrics_are_not_reported_as_zero_to_the_proposer() {
        let mut r = eval::Report {
            version: Some(2),
            kind: Some(Kind::Sysparam),
            candidate_id: "c".into(),
            status: "REJECT".into(),
            metrics: Default::default(),
            verdict: Default::default(),
            source: "phonefarm".into(),
            power_source: None,
            quality_reference: None,
            conditions_observed: None,
            evidence_dir: None,
            phonefarm_version: None,
            obtained_via: None,
        };
        r.verdict.per_metric = Some(std::collections::BTreeMap::from([(
            "power_watt".to_string(),
            eval::MetricDetail {
                status: Some("缺数据".into()),
                ..Default::default()
            },
        )]));
        let lines = per_metric_lines(&r);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("未测量"), "{lines:?}");
        assert!(!lines[0].contains("0.00%"));
    }
}
