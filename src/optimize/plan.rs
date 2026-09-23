//! 按摸底结果排一份计划: 开哪几层、每层跑几个候选、预算够不够。
//!
//! `--plan` 到这里就停 —— 不碰设备。计划本身是纯函数 (`build`),
//! 输入是 caps + 预算, 输出是步骤表, 所以同一份 caps 排出来的计划一样,
//! 可以拿录下来的 caps 做回归。
//!
//! 预算不够时**不偷偷缩轮数**。轮数决定统计判定的分辨率上限:
//! 每臂 5 轮时精确置换检验的最小可达 p 是 0.0079, 每臂 3 轮只有 0.1 ——
//! 悄悄砍到 3 轮, 换回来的是一整轮"什么都不显著"的真机时间, 而报告里
//! 看不出那是分辨率不够而不是候选没用。所以宁可少跑一层, 也不降轮数。

use super::caps::Caps;
use super::eval::{CarrierKind, ChargingSuspend, Conditions, Kind, Protocol};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// 每臂轮数的下限。
///
/// 这个数不是随手定的: n v n 精确置换检验的最小可达双侧 p = 2/C(2n,n),
/// n=5 时是 0.0079, 刚好够得着三指标 Bonferroni 收紧后的 0.0167;
/// n=4 时是 0.0286, 够不着。所以 5 是"判定还能有结论"的下限。
pub const MIN_ROUNDS: u32 = 5;

/// 等冷一次大概要多久 (秒)。用来估时间, 不是门槛。
const COOLDOWN_ESTIMATE_S: u64 = 90;
/// 每个候选的固定开销 (应用参数 / 还原 / 快照比对)。
const PER_CANDIDATE_OVERHEAD_S: u64 = 60;

/// 预算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub seconds: u64,
}

impl Budget {
    /// 默认预算刻意定小: 这条链路一跑就是几十分钟真机时间, 默认值应该是
    /// "先看看", 不是"跑通宵"。要跑长的必须显式说。
    pub const DEFAULT_SECONDS: u64 = 30 * 60;

    /// 解析 `90s` / `45m` / `2h` / 裸数字(按分钟)。
    pub fn parse(s: &str) -> Result<Self> {
        let t = s.trim();
        if t.is_empty() {
            bail!("空预算");
        }
        let (num, mult) = match t.chars().last().unwrap() {
            's' | 'S' => (&t[..t.len() - 1], 1),
            'm' | 'M' => (&t[..t.len() - 1], 60),
            'h' | 'H' => (&t[..t.len() - 1], 3600),
            _ => (t, 60),
        };
        let v: f64 = num
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("看不懂的预算 `{s}` (试试 45m / 2h / 900s)"))?;
        if !(v.is_finite() && v > 0.0) {
            bail!("预算必须是正数: `{s}`");
        }
        Ok(Self {
            seconds: (v * mult as f64).round() as u64,
        })
    }

    pub fn human(&self) -> String {
        let (h, m, s) = (
            self.seconds / 3600,
            (self.seconds % 3600) / 60,
            self.seconds % 60,
        );
        match (h, m, s) {
            (0, 0, s) => format!("{s}s"),
            (0, m, 0) => format!("{m}m"),
            (0, m, s) => format!("{m}m{s}s"),
            (h, 0, 0) => format!("{h}h"),
            (h, m, _) => format!("{h}h{m}m"),
        }
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            seconds: Self::DEFAULT_SECONDS,
        }
    }
}

/// 计划里的一步 = 一层。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub layer: Kind,
    pub layer_display: &'static str,
    pub route: String,
    /// 为什么开这一层。
    pub why: String,
    pub generations: u32,
    pub children: u32,
    /// 每臂轮数。
    pub rounds: u32,
    /// 单个候选估计耗时 (秒)。
    pub per_candidate_s: u64,
    /// 这一步估计耗时 (秒)。
    pub estimate_s: u64,
    /// 出主意的那一方: llm / local。
    pub proposer: String,
    /// 在什么载体上量。
    pub carrier: CarrierKind,
}

/// 没开的一层。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub layer: Kind,
    pub layer_display: &'static str,
    pub reason: String,
    pub blockers: Vec<String>,
}

/// 一份计划。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub schema: &'static str,
    pub generated_at: String,
    pub tool_version: &'static str,
    pub game: String,
    pub package: Option<String>,
    pub serial: String,
    pub budget_s: u64,
    pub steps: Vec<Step>,
    pub skipped: Vec<Skipped>,
    /// 估计总耗时 (秒)。
    pub estimate_s: u64,
    /// 冻结的测试条件, 三层共用。
    pub conditions: Conditions,
    /// 只测量不改动 —— 三层全关时为 true。
    pub measure_only: bool,
    /// 已知的坑, 跟着计划一起打印。
    pub pitfalls: Vec<super::games::Pitfall>,
}

/// 从 caps 推冻结的测试条件。
///
/// 等冷目标与温度上限都从**这次摸到的实际温度**推, 不用绝对常数:
/// 连跑几十轮之后设备根本降不到一个固定低温, 拿常数当门槛会把每组候选
/// 都卡死在等冷超时上。
pub fn conditions_from(caps: &Caps) -> Conditions {
    let base = caps.device.soc_temp_c.unwrap_or(40.0);
    // 温度上限: 安全线。超了这一组作废。
    let temp_cap = (base + 3.0).max(45.0);
    // 等冷目标: "回到基线是在什么热态下量的"。超时不作废, 安全由上限单独把关。
    let cool = (base + 1.0).max(40.0).min(temp_cap - 2.0);

    let suspend = caps.device.charging_suspend.is_true();
    Conditions {
        cool_c: Some(cool),
        temp_cap_c: Some(temp_cap),
        require_root: caps.device.root.is_true(),
        // 功耗只有在放电态才是整机功耗。停不了充也要求放电态, 是为了让接收端
        // 把 power_watt 如实留 null, 而不是报一个看着正常的假数字。
        require_discharging: true,
        charging_suspend: Some(ChargingSuspend {
            enabled: suspend,
            node: caps.device.charge_ctl_nodes.first().cloned(),
        }),
        // 风扇状态量不到。写 unknown 而不是猜一个 —— 猜错会让两批不可比的
        // 数据看起来可比。
        fan: "unknown".into(),
        // 游戏内帧率设置同理: 这一版还读不到, 如实留空, 由人补。
        ingame_fps_setting: None,
        screen_brightness: None,
        notes: None,
    }
}

/// 一层在这台设备上该在什么载体上量。
fn carrier_for(kind: Kind, caps: &Caps) -> CarrierKind {
    match kind {
        // 系统参数与画面流程改写都必须在**真实负载**里量 —— 它们改的是
        // 整机行为, 孤立工装量不出来。
        Kind::Sysparam | Kind::Gray => match caps.carrier.box_kind {
            super::games::BoxKind::WhiteBox => CarrierKind::Refbench,
            super::games::BoxKind::BlackBox => CarrierKind::GameReplay,
        },
        Kind::Shader => match caps.carrier.box_kind {
            super::games::BoxKind::WhiteBox => CarrierKind::Refbench,
            super::games::BoxKind::BlackBox => CarrierKind::GameReplay,
        },
    }
}

/// 单轮负载时长 (秒)。
fn workload_seconds(caps: &Caps) -> u64 {
    match caps.carrier.box_kind {
        super::games::BoxKind::WhiteBox => 40,
        super::games::BoxKind::BlackBox => caps.target.workload.seconds.max(20) as u64,
    }
}

/// 排计划。纯函数。
pub fn build(caps: &Caps, budget: Budget) -> Plan {
    let seconds = workload_seconds(caps);
    // 一对 A/B = 两次"等冷 + 跑负载"。
    let per_pair_s = 2 * (COOLDOWN_ESTIMATE_S + seconds);
    let per_candidate_s = per_pair_s * MIN_ROUNDS as u64 + PER_CANDIDATE_OVERHEAD_S;

    // 开路顺序: ③ → ② → ①。不是随便排的 ——
    // 系统参数不动画面 (不必量画质, 最便宜也最安全), 画面流程改写次之,
    // 画面算子最贵 (要编译、要注入、要补测画质)。便宜的先跑, 预算见底时
    // 砍掉的是最贵的那一层, 而不是最有希望的那一层。
    let order: [(Kind, &super::caps::LayerVerdict); 3] = [
        (Kind::Sysparam, &caps.layers.sysparam),
        (Kind::Gray, &caps.layers.gray),
        (Kind::Shader, &caps.layers.shader),
    ];

    let mut steps = Vec::new();
    let mut skipped = Vec::new();
    let open: Vec<&(Kind, &super::caps::LayerVerdict)> =
        order.iter().filter(|(_, v)| v.open).collect();
    let share = if open.is_empty() {
        0
    } else {
        budget.seconds / open.len() as u64
    };

    for (kind, verdict) in order.iter() {
        if !verdict.open {
            skipped.push(Skipped {
                layer: *kind,
                layer_display: kind.display(),
                reason: verdict.reason.clone(),
                blockers: verdict.blockers.clone(),
            });
            continue;
        }
        // 这一层的份额里塞得下几个候选。轮数不降 —— 降了判定就出不了结论。
        let fits = share / per_candidate_s.max(1);
        if fits == 0 {
            skipped.push(Skipped {
                layer: *kind,
                layer_display: kind.display(),
                reason: format!(
                    "预算不够: 这一层分到 {}, 但一个候选要 {} (每臂 {} 轮不能再少, 否则判定够不着显著性)",
                    Budget { seconds: share }.human(),
                    Budget {
                        seconds: per_candidate_s
                    }
                    .human(),
                    MIN_ROUNDS
                ),
                blockers: vec![format!(
                    "想跑这一层, 预算至少要 {}",
                    Budget {
                        seconds: per_candidate_s * open.len() as u64
                    }
                    .human()
                )],
            });
            continue;
        }
        // 候选先铺宽再迭代深: 第一代什么都不知道, 多试几个方向比在一个方向上
        // 多迭代一代更划算。
        let children = fits.clamp(1, 3) as u32;
        let generations = (fits / children as u64).max(1) as u32;
        steps.push(Step {
            layer: *kind,
            layer_display: kind.display(),
            route: verdict.route.clone().unwrap_or_default(),
            why: verdict.reason.clone(),
            generations,
            children,
            rounds: MIN_ROUNDS,
            per_candidate_s,
            estimate_s: per_candidate_s * (children as u64) * (generations as u64),
            proposer: "llm (挂了退回本地变异器)".into(),
            carrier: carrier_for(*kind, caps),
        });
    }

    let estimate_s = steps.iter().map(|s| s.estimate_s).sum();
    Plan {
        schema: "game_opt_loop/plan@1",
        generated_at: crate::engine::evolution::now(),
        tool_version: crate::VERSION,
        game: caps.target.display.clone(),
        package: caps.target.package.clone(),
        serial: caps.device.serial.clone(),
        budget_s: budget.seconds,
        steps,
        skipped,
        estimate_s,
        conditions: conditions_from(caps),
        measure_only: !caps.any_layer_open(),
        pitfalls: caps.target.pitfalls.clone(),
    }
}

/// 一层的冻结统计协议。
pub fn protocol_for(step: &Step, caps: &Caps) -> Protocol {
    Protocol::for_kind(
        step.layer,
        step.rounds,
        workload_seconds(caps) as u32,
        conditions_from(caps).cool_c.unwrap_or(40.0),
    )
}

/// 把计划渲染成给人看的文本。
pub fn render(plan: &Plan, caps: &Caps) -> String {
    let mut o = String::new();
    o.push_str(&format!(
        "目标: {} ({})\n",
        plan.game,
        plan.package.as_deref().unwrap_or("包名未知")
    ));
    o.push_str(&format!(
        "设备: {} {} / {}\n",
        plan.serial,
        caps.device.model.as_deref().unwrap_or("型号未知"),
        match caps.device.platform {
            super::caps::Platform::Adb => "Android (adb)",
            super::caps::Platform::Hdc => "OpenHarmony (hdc)",
            super::caps::Platform::Unknown => "平台未知",
        }
    ));
    o.push_str(&format!(
        "  root {} | 停充 {} | 挂层 {} | 目标已装 {}\n",
        yn(&caps.device.root),
        yn(&caps.device.charging_suspend),
        yn(&caps.device.vulkan_layer),
        yn(&caps.device.game_installed),
    ));
    o.push_str(&format!(
        "  电池 {} {}% | SoC {}\n",
        caps.device.battery.status.as_deref().unwrap_or("未知"),
        caps.device
            .battery
            .capacity_pct
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into()),
        caps.device
            .soc_temp_c
            .map(|t| format!("{t:.1}C"))
            .unwrap_or_else(|| "未测到".into()),
    ));
    o.push_str(&format!(
        "  载体: {}\n",
        match caps.carrier.box_kind {
            super::games::BoxKind::WhiteBox => format!(
                "白盒 — {}",
                caps.carrier
                    .carrier
                    .as_ref()
                    .map(|c| c.dir.clone())
                    .unwrap_or_default()
            ),
            super::games::BoxKind::BlackBox => "黑盒 (第三方游戏)".to_string(),
        }
    ));

    o.push_str(&format!(
        "\n预算 {} | 估计耗时 {}\n",
        Budget {
            seconds: plan.budget_s
        }
        .human(),
        Budget {
            seconds: plan.estimate_s
        }
        .human()
    ));

    if plan.measure_only {
        o.push_str("\n三层全都开不了 —— 这一轮只测量, 不改动设备。原因见下。\n");
    }

    o.push_str("\n要跑的层:\n");
    if plan.steps.is_empty() {
        o.push_str("  (没有)\n");
    }
    for s in &plan.steps {
        o.push_str(&format!(
            "  {} — {}\n    路线: {}\n    {} 代 x {} 候选, 每臂 {} 轮, 载体 {:?}\n    单候选约 {}, 小计约 {}\n",
            s.layer_display,
            s.why,
            s.route,
            s.generations,
            s.children,
            s.rounds,
            s.carrier,
            Budget {
                seconds: s.per_candidate_s
            }
            .human(),
            Budget {
                seconds: s.estimate_s
            }
            .human(),
        ));
    }

    if !plan.skipped.is_empty() {
        o.push_str("\n不跑的层:\n");
        for s in &plan.skipped {
            o.push_str(&format!("  {} — {}\n", s.layer_display, s.reason));
            for b in &s.blockers {
                o.push_str(&format!("    还差: {b}\n"));
            }
        }
    }

    let c = &plan.conditions;
    o.push_str("\n冻结的测试条件 (看到任何候选数据之前就定下, 事后不改):\n");
    o.push_str(&format!(
        "  等冷目标 {} | 温度上限 {} | 要 root {} | 要放电态 {}\n",
        c.cool_c.map(|v| format!("{v:.1}C")).unwrap_or("不等冷".into()),
        c.temp_cap_c
            .map(|v| format!("{v:.1}C"))
            .unwrap_or("无".into()),
        if c.require_root { "是" } else { "否" },
        if c.require_discharging { "是" } else { "否" },
    ));
    o.push_str(&format!(
        "  停充 {} | 风扇 {} | 游戏内帧率设置 {}\n",
        match &c.charging_suspend {
            Some(s) if s.enabled => format!(
                "会尝试 ({})",
                s.node.as_deref().unwrap_or("节点由接收端自己探")
            ),
            _ => "停不了 —— 充电态下功耗如实留空, 不拿假数字比".to_string(),
        },
        c.fan,
        c.ingame_fps_setting.as_deref().unwrap_or("未记录 (要人补)"),
    ));

    if !plan.pitfalls.is_empty() {
        o.push_str("\n这台机器 / 这个目标上的已知坑:\n");
        for p in &plan.pitfalls {
            o.push_str(&format!("  - {}\n    → {}\n", p.symptom, p.workaround));
        }
    }
    o
}

fn yn(p: &super::caps::Probe<bool>) -> &'static str {
    match p.value {
        Some(true) => "有",
        Some(false) => "无",
        None => "没探出来",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimize::caps;
    use crate::optimize::games::{self, CarrierRoots};
    use std::path::PathBuf;

    fn caps_for(raw: &str) -> Caps {
        let t = games::resolve(
            "原神",
            None,
            &CarrierRoots {
                carriers_dir: PathBuf::from("/nonexistent"),
                siblings_dir: PathBuf::from("/nonexistent"),
            },
        );
        caps::assemble(&t, "S1", raw)
    }

    const ROOTED: &str = "model=NX809J\nroot_uid=0\nvklayers=\nbattery_status=Discharging\n\
battery_capacity=84\nbattery_current_now=-1200000\n\
charge_node=/sys/class/qcom-battery/charging_enabled\ntemp_mc=41300\n\
pkg_path=package:/data/app/x/base.apk\n";

    const UNROOTED: &str = "model=Pixel\nroot_uid=NA\nbattery_status=Charging\ntemp_mc=38000\n\
pkg_path=package:/data/app/x/base.apk\n";

    #[test]
    fn budget_parsing() {
        assert_eq!(Budget::parse("90s").unwrap().seconds, 90);
        assert_eq!(Budget::parse("45m").unwrap().seconds, 2700);
        assert_eq!(Budget::parse("2h").unwrap().seconds, 7200);
        assert_eq!(Budget::parse("30").unwrap().seconds, 1800, "裸数字按分钟");
        assert_eq!(Budget::parse("1.5h").unwrap().seconds, 5400);
        for bad in ["", "abc", "-5m", "0"] {
            assert!(Budget::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn budget_is_small_by_default() {
        // 默认值应该是"先看看", 不是"跑通宵"。
        assert!(Budget::default().seconds <= 60 * 60);
    }

    /// 计划是 caps 的纯函数 —— 同一份 caps 排两次, 除时间戳外逐字段相同。
    #[test]
    fn the_plan_is_a_pure_function_of_caps() {
        let c = caps_for(ROOTED);
        let a = build(&c, Budget::parse("3h").unwrap());
        let b = build(&c, Budget::parse("3h").unwrap());
        assert_eq!(
            serde_json::to_value(&a.steps).unwrap(),
            serde_json::to_value(&b.steps).unwrap()
        );
    }

    #[test]
    fn a_rooted_device_plans_all_three_layers_when_the_budget_allows() {
        let plan = build(&caps_for(ROOTED), Budget::parse("6h").unwrap());
        assert_eq!(plan.steps.len(), 3, "{:?}", plan.skipped);
        // 顺序: 便宜且不动画面的先跑。
        assert_eq!(plan.steps[0].layer, Kind::Sysparam);
        assert_eq!(plan.steps[1].layer, Kind::Gray);
        assert_eq!(plan.steps[2].layer, Kind::Shader);
        assert!(!plan.measure_only);
    }

    /// 预算不够时砍层, **不降轮数** —— 降了判定就够不着显著性,
    /// 换回来的是一整轮"什么都不显著"。
    #[test]
    fn a_tight_budget_drops_layers_instead_of_rounds() {
        let plan = build(&caps_for(ROOTED), Budget::parse("10m").unwrap());
        for s in &plan.steps {
            assert_eq!(s.rounds, MIN_ROUNDS);
        }
        assert!(!plan.skipped.is_empty(), "预算这么小不该三层都跑");
        // 被砍掉的层必须说清楚差多少。
        for s in &plan.skipped {
            assert!(!s.reason.is_empty());
        }
    }

    #[test]
    fn nothing_open_means_measure_only() {
        let plan = build(&caps_for(UNROOTED), Budget::parse("6h").unwrap());
        assert!(plan.steps.is_empty());
        assert!(plan.measure_only);
        assert_eq!(plan.skipped.len(), 3);
    }

    /// 等冷目标与温度上限从实测温度推, 不是常数。
    #[test]
    fn thermal_targets_follow_the_measured_temperature() {
        let hot = caps_for(&ROOTED.replace("temp_mc=41300", "temp_mc=52000"));
        let c = conditions_from(&hot);
        assert_eq!(c.temp_cap_c, Some(55.0));
        assert_eq!(c.cool_c, Some(53.0));

        // 凉机上有下限兜底, 不会把目标定到一个永远等不到的低温。
        let cool = caps_for(&ROOTED.replace("temp_mc=41300", "temp_mc=25000"));
        let c = conditions_from(&cool);
        assert_eq!(c.temp_cap_c, Some(45.0));
        assert_eq!(c.cool_c, Some(40.0));
        assert!(c.cool_c.unwrap() < c.temp_cap_c.unwrap());
    }

    /// 停不了充也要求放电态 —— 为的是让接收端把功耗留 null, 而不是报假数字。
    #[test]
    fn power_is_always_required_to_be_measured_on_battery() {
        for raw in [ROOTED, UNROOTED] {
            assert!(conditions_from(&caps_for(raw)).require_discharging);
        }
        // 没有 root 就停不了充, 条件里要如实反映。
        let c = conditions_from(&caps_for(UNROOTED));
        assert!(!c.charging_suspend.unwrap().enabled);
    }

    /// 风扇状态量不到就写 unknown, 不猜。
    #[test]
    fn the_fan_state_is_never_guessed() {
        assert_eq!(conditions_from(&caps_for(ROOTED)).fan, "unknown");
    }

    /// 渲染出来的计划必须把"为什么不跑这一层"讲出来 ——
    /// 那是用户唯一能拿到的交代。
    #[test]
    fn the_rendered_plan_explains_every_closed_layer() {
        let c = caps_for(UNROOTED);
        let text = render(&build(&c, Budget::default()), &c);
        assert!(text.contains("不跑的层"));
        assert!(text.contains("root"));
        assert!(text.contains("冻结的测试条件"));
        // 已知的坑也要跟着出现。
        assert!(text.contains("手柄"), "{text}");
    }
}
