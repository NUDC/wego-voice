//! 房间噪声本底跟踪与人声门限。
//!
//! # 解决的是什么问题
//!
//! 噪声对这个产品的真正伤害**不在监听，在音高检测**。
//!
//! 用户抱怨的不会是"我听到一点底噪"，而是"修音一会儿有一会儿没有" ——
//! 那是噪声抬高了 YIN 的 aperiodicity、把浊音误判成清音，
//! PSOLA 转成透传的症状。反过来，风扇、变压器电流声这类**有周期成分**的
//! 噪声可能被判成浊音，于是 PSOLA 开始对着风扇修音。
//!
//! 原先的判据是一个写死的常数 `YinConfig::silence_rms = 1e-4`（≈ -80 dBFS）。
//! 实测这台机器 WASAPI 独占下的输入本底是 0.0017（≈ -55 dBFS）——
//! **阈值比真实本底低了 25 dB，等于形同虚设**。
//!
//! # 做法：VAD 冻结的本底跟踪
//!
//! 这是实时通话产品（WebRTC / 各家 RTC 引擎）里的公版做法：
//! **在没有人声的帧更新噪声估计，有人声时冻结**。
//!
//! 冻结是关键。唱歌是持续信号，一个持续 5 秒的长音会把任何
//! "无脑跟随"的本底估计抬到人声电平上去，然后门就永远打不开了。
//!
//! 再加一条非对称时间常数：
//!
//! - **降得快**（τ≈150ms）：从吵的环境换到安静环境，几乎立刻跟上
//! - **升得慢**（τ≈3s）：`s` `f` `sh` 这类清辅音虽然响且非周期，
//!   但持续时间通常 < 100ms，抬不动本底
//!
//! # 为什么不做成频域
//!
//! 频域噪声谱（谱减/维纳）更强，但那是下一步的事，而且只能放在
//! **分析路径**上（监听路径只剩 0.08ms 预算）。
//! 这个模块刻意只做标量本底 —— 它是频域版本的前置：
//! 门限判据本身不变，变的只是"噪声估计"从一个数变成一条谱。

/// 绝对下限。低于这个电平认为是数字静音（设备没在送数据、或者真的全零）。
///
/// 这一条**不是**噪声门，是防御：本底估计降到 0 之后，
/// `rms > floor * margin` 会恒成立，门就永远开着了。
const ABSOLUTE_FLOOR: f32 = 1e-5;

/// 启动观察期（秒）。这段时间内只估本底、不放行任何浊音判定。
///
/// 没有这一段的话，程序刚起来第一帧就可能被当成人声 ——
/// 而那一帧极可能是设备启动的瞬态（独占模式协商完成时的爆音）。
const SETTLE_SECS: f32 = 0.3;

/// 预热期（秒）：完全忽略。
///
/// 调用方的分析缓冲刚开始是全零，要几十毫秒才填满真实音频。
/// 拿半满的缓冲去估本底会得到一个荒谬的低值，门随即形同虚设。
const PRIME_SECS: f32 = 0.05;

/// "噪声"本底的合理上限（≈ -34 dBFS）。超过它说明估出来的根本不是噪声。
///
/// # 这一条是整个模块最重要的防线
///
/// 最初的版本没有它，结果是：**用户启动引擎时如果已经在唱，
/// 本底就把歌声学了进去，门从此再也打不开。**
///
/// 一旦本底高过这个值，我们判定"估不准"，于是**门直接失效、一律放行**，
/// 退回到 YIN 自己的 aperiodicity 判据 —— 也就是加这个模块之前的行为。
///
/// **失败要朝开的方向倒。** 噪声门是个增强手段，绝不能变成挡住用户唱歌的东西：
/// 漏掉一次风扇误触发只是小瑕疵，把人声整段吞掉是产品事故。
const MAX_PLAUSIBLE_FLOOR: f32 = 0.02;

#[derive(Debug, Clone, Copy)]
pub struct NoiseGateConfig {
    pub sample_rate: f32,
    /// 高出本底多少 dB 才认为"可能是人声"。
    ///
    /// 12 dB 是个保守起点：安静房间里说话比本底高 25~40 dB，
    /// 定得再高会吃掉弱起音和收尾的气声。
    pub margin_db: f32,
    /// 本底下降的时间常数（秒）。换到安静环境要跟得上。
    pub fall_secs: f32,
    /// 本底上升的时间常数（秒）。必须远大于清辅音时长。
    pub rise_secs: f32,
}

impl Default for NoiseGateConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            margin_db: 12.0,
            fall_secs: 0.15,
            rise_secs: 3.0,
        }
    }
}

/// 噪声本底跟踪器。
///
/// **实时安全**：无分配、无锁、无 panic。每个分析帧调用一次。
#[derive(Debug, Clone)]
pub struct NoiseGate {
    cfg: NoiseGateConfig,
    /// 当前噪声本底估计（线性 RMS）。
    floor: f32,
    /// `10^(margin_db/20)`，预算好省得每帧算 powf。
    margin: f32,
    /// 已观察的样本数，用于预热期与启动观察期。
    observed: u64,
    prime_samples: u64,
    settle_samples: u64,
    /// 观察期内是否已取到过一个最小值。
    seeded: bool,
}

impl NoiseGate {
    pub fn new(cfg: NoiseGateConfig) -> Self {
        Self {
            margin: 10f32.powf(cfg.margin_db / 20.0),
            prime_samples: (cfg.sample_rate * PRIME_SECS) as u64,
            settle_samples: (cfg.sample_rate * SETTLE_SECS) as u64,
            floor: ABSOLUTE_FLOOR,
            observed: 0,
            seeded: false,
            cfg,
        }
    }

    /// 改门限余量（dB）。运行时可调，UI 上是一根滑杆。
    pub fn set_margin_db(&mut self, db: f32) {
        self.cfg.margin_db = db.clamp(0.0, 40.0);
        self.margin = 10f32.powf(self.cfg.margin_db / 20.0);
    }

    #[inline]
    pub fn margin_db(&self) -> f32 {
        self.cfg.margin_db
    }

    /// 当前噪声本底（线性 RMS）。诊断页显示它，用来判断房间/设备够不够安静。
    #[inline]
    pub fn floor(&self) -> f32 {
        self.floor
    }

    /// 当前放行门限（线性 RMS）。
    #[inline]
    pub fn threshold(&self) -> f32 {
        self.floor * self.margin
    }

    /// 启动观察期是否已过。
    #[inline]
    pub fn settled(&self) -> bool {
        self.observed >= self.settle_samples
    }

    /// 本底估计是否可信 —— 也就是噪声门到底生不生效。
    ///
    /// 不可信有两种情况，处理方式相同（放行）：
    /// - 还在启动观察期
    /// - 估出来的"本底"高过 [`MAX_PLAUSIBLE_FLOOR`]，说明学进去的是信号不是噪声
    ///
    /// 诊断页应当显示这个状态：用户需要知道门有没有在工作，
    /// 以及"没在工作"是因为房间太吵还是刚启动。
    #[inline]
    pub fn confident(&self) -> bool {
        self.settled() && self.floor <= MAX_PLAUSIBLE_FLOOR
    }

    /// 本帧电平是否够格进入音高检测。
    ///
    /// 门关着时调用方**应当跳过 YIN** —— 既是正确性（不对着风扇修音），
    /// 也是白赚的 CPU：静音段占了实际使用时间的一大半。
    ///
    /// ⚠️ 启动观察期内一律**关**（挡住独占模式协商完成时的爆音）；
    /// 观察期过后若本底不可信则一律**开**（见 [`MAX_PLAUSIBLE_FLOOR`]）。
    #[inline]
    pub fn is_open(&self, rms: f32) -> bool {
        if !self.settled() {
            return false;
        }
        if self.floor > MAX_PLAUSIBLE_FLOOR {
            return true; // 估不准就别拦
        }
        rms > self.threshold()
    }

    /// 用一个"确认不是人声"的帧更新本底。
    ///
    /// ⚠️ **只在 VAD 判定为非人声时调用。** 人声帧必须冻结 ——
    /// 持续音会把本底一路抬到人声电平，门就再也开不了了。
    pub fn update(&mut self, rms: f32, hop_samples: usize) {
        // 数字静音 = 设备还没开始送数据，不是"房间很安静"。
        //
        // 不排掉的话：启动瞬间采集流尚未出数据，下面的最小值跟踪会把 0
        // 当成房间本底学进去 → 本底钉死在 1e-5 → 门限降到 -88 dBFS →
        // 门永远开着；此时只要 YIN 把某段噪声判成浊音，本底就被**永久冻结**
        // 在这个错值上，再也回不来。
        //
        // 实测抓到的：cpal 共享模式启动较慢，三轮里有两轮读出正好 -100 dBFS。
        //
        // 真实房间噪声不可能恰好是 0，所以这一条不会误伤。
        // 输入真的全零时 `settled()` 永远不成立、门保持关闭 —— 那也是对的，
        // 全零信号里没有音高可测，顺带省掉 YIN 的开销。
        if rms <= ABSOLUTE_FLOOR {
            return;
        }

        self.observed = self.observed.saturating_add(hop_samples as u64);
        let x = rms;

        // 预热期：分析缓冲还没填满真实音频，这时候的 rms 没有意义
        if self.observed < self.prime_samples {
            return;
        }

        // 观察期：取最小值，而不是平滑。
        //
        // 平滑会被启动瞬态或"一开机就在唱"带偏；取最小值只要这 300ms 里
        // 有过任意一个安静瞬间，就能拿到正确的本底。
        // 一个都没有的话，floor 会停在信号电平上 —— 那正是
        // `confident()` 要识别并放行的情况。
        if !self.settled() {
            self.floor = if self.seeded { self.floor.min(x) } else { x };
            self.seeded = true;
            return;
        }

        // 稳态：非对称一阶平滑，降得快、升得慢
        let tau = if x < self.floor {
            self.cfg.fall_secs
        } else {
            self.cfg.rise_secs
        };
        let a = one_pole(tau, self.cfg.sample_rate, hop_samples);
        self.floor += (x - self.floor) * a;
        self.floor = self.floor.max(ABSOLUTE_FLOOR);
    }

    /// 设备或参数变了，重来一遍。
    pub fn reset(&mut self) {
        self.floor = ABSOLUTE_FLOOR;
        self.observed = 0;
        self.seeded = false;
    }
}

/// 一阶低通系数：每 `hop` 个样本更新一次，时间常数 `tau` 秒。
#[inline]
fn one_pole(tau_secs: f32, sample_rate: f32, hop: usize) -> f32 {
    if tau_secs <= 0.0 {
        return 1.0;
    }
    let dt = hop as f32 / sample_rate;
    1.0 - (-dt / tau_secs).exp()
}

/// 线性幅度转 dBFS。UI 与报告用 —— 人对噪声的直觉是 dB，不是 0.0017。
pub fn to_dbfs(linear: f32) -> f32 {
    20.0 * linear.max(1e-9).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const HOP: usize = 256;

    fn gate() -> NoiseGate {
        NoiseGate::new(NoiseGateConfig { sample_rate: SR, ..Default::default() })
    }

    /// 喂 `secs` 秒的恒定电平。
    fn feed(g: &mut NoiseGate, rms: f32, secs: f32) {
        let hops = (secs * SR / HOP as f32) as usize;
        for _ in 0..hops {
            g.update(rms, HOP);
        }
    }

    #[test]
    fn settles_to_the_room_floor() {
        let mut g = gate();
        feed(&mut g, 0.0017, 1.0); // 实测本底
        assert!(
            (g.floor() - 0.0017).abs() < 0.0002,
            "本底估计为 {:.5}，期望约 0.0017",
            g.floor()
        );
    }

    /// 启动观察期内一律不放行 —— 独占模式协商完成时的瞬态会被挡住。
    #[test]
    fn stays_shut_during_settle() {
        let mut g = gate();
        g.update(0.5, HOP);
        assert!(!g.is_open(0.5), "观察期未过就放行了");
        feed(&mut g, 0.001, 0.5);
        assert!(g.settled());
    }

    #[test]
    fn opens_only_above_the_margin() {
        let mut g = gate();
        feed(&mut g, 0.002, 1.0);
        // 默认 12 dB ≈ ×3.98
        assert!(!g.is_open(0.002 * 3.0), "只高出 9.5 dB 就放行了");
        assert!(g.is_open(0.002 * 6.0), "高出 15.6 dB 却没放行");
    }

    /// 这条是整个模块存在的理由。
    ///
    /// 不冻结的话，一个持续长音会把本底一路抬到人声电平，
    /// 门随即关死 —— 表现就是"唱着唱着修音没了"。
    #[test]
    fn a_held_note_must_not_lift_the_floor() {
        let mut g = gate();
        feed(&mut g, 0.002, 1.0);
        let before = g.floor();

        // 唱 5 秒。调用方判定为浊音 → 一次 update 都不调（这就是冻结）
        let sung = 0.2;
        assert!(g.is_open(sung), "正常演唱电平应当放行");
        assert_eq!(g.floor(), before, "冻结期间本底不该动");
        assert!(g.is_open(sung), "5 秒后仍应放行");
    }

    /// 清辅音（s / f / sh）响且非周期，会被 VAD 判成非人声而参与更新。
    /// 但它们很短，非对称时间常数必须让它们抬不动本底。
    #[test]
    fn brief_fricatives_barely_move_the_floor() {
        let mut g = gate();
        feed(&mut g, 0.002, 2.0);
        let before = g.floor();

        // 80ms 的清辅音，电平比本底高 20 dB
        feed(&mut g, 0.02, 0.08);

        // 实测抬升 ≈ 1.8 dB。门限余量是 12 dB，还剩足够裕度；
        // 而且下一段静音会以 0.15s 的时间常数把它拉回去。
        let lift_db = to_dbfs(g.floor()) - to_dbfs(before);
        assert!(lift_db < 2.5, "80ms 清辅音把本底抬了 {lift_db:.2} dB");
    }

    /// 反过来：环境真的持续变吵了，本底必须跟上，否则门会一直开着。
    #[test]
    fn a_sustained_noise_rise_is_eventually_tracked() {
        let mut g = gate();
        feed(&mut g, 0.002, 2.0);
        feed(&mut g, 0.02, 12.0); // 空调开了，持续 12 秒

        let lift_db = to_dbfs(g.floor()) - to_dbfs(0.002);
        assert!(lift_db > 12.0, "持续 12 秒的噪声只把本底抬了 {lift_db:.1} dB");
    }

    /// 换到安静环境要跟得快，否则门会被旧的高本底堵住。
    #[test]
    fn falls_fast_when_the_room_gets_quiet() {
        let mut g = gate();
        feed(&mut g, 0.02, 3.0);
        feed(&mut g, 0.0005, 1.0);
        assert!(
            g.floor() < 0.001,
            "1 秒后本底仍有 {:.5}，降得太慢",
            g.floor()
        );
    }

    /// 本底不能降到 0 —— 否则 `rms > floor * margin` 恒成立，门形同虚设。
    #[test]
    fn never_collapses_to_zero() {
        let mut g = gate();
        feed(&mut g, 0.0, 5.0);
        assert!(g.floor() >= ABSOLUTE_FLOOR);
        assert!(!g.is_open(0.0));
    }

    /// **回归用例：启动时设备还没开始送数据。**
    ///
    /// 实测在 cpal 共享模式下抓到：三轮有两轮把本底学成了正好 -100 dBFS
    /// （绝对下限），因为观察期内采集流还在预热、送的是数字静音。
    ///
    /// 后果比看上去严重：本底钉死在最低值 → 门永远开着 →
    /// 一旦 YIN 把某段噪声判成浊音，本底就被永久冻结在错值上。
    #[test]
    fn digital_silence_at_startup_must_not_become_the_floor() {
        let mut g = gate();
        feed(&mut g, 0.0, 0.5); // 设备还没醒
        assert!(!g.settled(), "全是数字静音却认为观察期已完成");

        feed(&mut g, 0.002, 0.5); // 数据来了
        assert!(g.settled());
        assert!(
            (g.floor() - 0.002).abs() < 0.0003,
            "本底学成了 {:.5}，期望约 0.002",
            g.floor()
        );
        assert!(!g.is_open(0.004), "本底偏低导致门拦不住噪声");
    }

    /// **回归用例：引擎启动时用户已经在唱。**
    ///
    /// 第一版没有 `MAX_PLAUSIBLE_FLOOR`，本底把歌声学了进去，
    /// 门从此永远打不开 —— 用户的表现是"启动完一点反应都没有"。
    ///
    /// 正确行为是判定"估不准"并**放行**，退回 YIN 自己的判据。
    #[test]
    fn already_singing_at_startup_must_not_lock_the_gate_shut() {
        let mut g = gate();
        let singing = 0.5;
        feed(&mut g, singing, 2.0); // 从第一帧起就一直在唱

        assert!(!g.confident(), "把歌声当成了噪声本底却还自称可信");
        assert!(g.is_open(singing), "门被锁死了 —— 用户会以为程序坏了");
        assert!(g.is_open(0.01), "不可信时应当一律放行");
    }

    /// 承接上一条：用户一停下来，本底必须迅速回到真实值，门恢复工作。
    #[test]
    fn recovers_once_the_singing_stops() {
        let mut g = gate();
        feed(&mut g, 0.5, 2.0);
        assert!(!g.confident());

        feed(&mut g, 0.0017, 1.5); // 停下来喘口气
        assert!(g.confident(), "安静 1.5 秒后仍未恢复可信");
        assert!(!g.is_open(0.003), "恢复后门却拦不住噪声");
    }

    /// 房间真的太吵（本底高过合理上限）时，门应当自己让位，而不是把人声吞掉。
    #[test]
    fn a_hopeless_room_disables_the_gate_instead_of_blocking() {
        let mut g = gate();
        feed(&mut g, 0.05, 3.0); // ≈ -26 dBFS 的本底，没救了
        assert!(!g.confident());
        assert!(g.is_open(0.05), "吵房间里应当放行，交给 YIN 自己判");
    }

    #[test]
    fn dbfs_conversion() {
        assert!((to_dbfs(1.0) - 0.0).abs() < 1e-4);
        assert!((to_dbfs(0.5) + 6.02).abs() < 0.01);
        assert!(to_dbfs(0.0).is_finite(), "静音不能算出 -inf");
    }
}
