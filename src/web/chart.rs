//! 服务端渲染的轻量 SVG 图表（无前端图表依赖，离线可用）。
//!
//! 支持两种线型：折线（默认）与方波（`step`，值保持到下一次上报，适合
//! 阶跃式遥测）；纵轴刻度可按序列要求取整（如温度的整度刻度）；
//! 单图可叠加多条序列（共享时间轴与纵轴，如电源/UPS 电压对比）。

pub struct Series {
    pub label: String,
    pub unit: String,
    pub color: String,
    pub points: Vec<(i64, f64)>,
    /// 纵轴刻度小数位：`None` = 按数据自动；`Some(0)` = 整数刻度
    /// （温度这类设备只上报整值的物理量，避免出现 62.1 / 61.8 这类刻度）。
    pub tick_decimals: Option<u32>,
    /// 方波（阶梯）线型：`true` 时值保持到下一个时间点，而不是直线插值。
    pub step: bool,
    /// 固定纵轴范围（如电量恒为 0~100%）；`None` = 按数据自动外扩。
    pub fixed_range: Option<(f64, f64)>,
    /// 纵轴下界（强制）。电压这类物理量最低就是 0，纵轴一律从 0 起：
    /// 既不会外扩出负刻度，也让 0 这条基线始终留在视野里；`None` = 按数据自动。
    pub axis_floor: Option<f64>,
}

/// 关键事件类型（电池曲线标注用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// 充电开始（电量由降/平转升）
    Charge,
    /// 放电开始（电量由升/平转降）
    Discharge,
    /// 充满（电量越过 ~99%）
    Full,
}

impl EventKind {
    /// 颜色：与图例/事件列表保持一致。
    pub fn color(&self) -> &'static str {
        match self {
            EventKind::Charge => "#059669",
            EventKind::Discharge => "#f5a524",
            EventKind::Full => "#1d4ed8",
        }
    }
}

/// 一个需要在时间轴上标注的关键事件点。
#[derive(Debug, Clone)]
pub struct ChartEvent {
    pub t: i64,
    pub kind: EventKind,
}

const W: f64 = 640.0;
const H: f64 = 200.0;
const PAD_L: f64 = 46.0;
const PAD_R: f64 = 14.0;
const PAD_T: f64 = 16.0;
const PAD_B: f64 = 28.0;

/// 深色控制台配色：网格/文字/图例分别取低、中、高三档亮度。
pub const GRID: &str = "#1b3550";
pub const AXIS_TEXT: &str = "#6f8fb3";
pub const LEGEND_TEXT: &str = "#a9c2dc";
pub const HINT_TEXT: &str = "#5d7d9e";

pub fn line_chart(series: &Series, empty_hint: &str) -> String {
    line_chart_multi(std::slice::from_ref(series), &[], empty_hint)
}

/// 单序列图，并叠加事件标注（电池曲线用来标出充电开始 / 充满等变化点）。
pub fn line_chart_events(series: &Series, events: &[ChartEvent], empty_hint: &str) -> String {
    line_chart_multi(std::slice::from_ref(series), events, empty_hint)
}

/// 多序列图：共享时间横轴与数值纵轴，图例逐条列出。
/// `events` 为可选的关键事件标注（竖向虚线 + 顶部色点 + 短时分标签）。
pub fn line_chart_multi(series: &[Series], events: &[ChartEvent], empty_hint: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        r##"<svg class="chart" viewBox="0 0 {W} {H}" width="100%" height="{H}" preserveAspectRatio="xMidYMid meet" role="img">"##
    ));
    out.push_str(r##"<rect x="0" y="0" width="100%" height="100%" fill="transparent"/>"##);

    let has_points = series.iter().any(|s| !s.points.is_empty());
    if !has_points {
        out.push_str(&format!(
            r##"<text x="{}" y="{}" text-anchor="middle" fill="{HINT_TEXT}" font-size="13">{}</text>"##,
            W / 2.0,
            H / 2.0,
            escape(empty_hint)
        ));
        out.push_str("</svg>");
        return out;
    }

    // ---- 纵轴范围：任一序列声明固定范围则采用；否则取全体数据合并外扩 ----
    let int_ticks = series.iter().any(|s| s.tick_decimals == Some(0));
    let (mut dmin, mut dmax) = (f64::MAX, f64::MIN);
    for s in series {
        for (_, v) in &s.points {
            dmin = dmin.min(*v);
            dmax = dmax.max(*v);
        }
    }
    if (dmax - dmin).abs() < f64::EPSILON {
        dmin -= 1.0;
        dmax += 1.0;
    }
    let (lo, hi, ticks) = match series.iter().find_map(|s| s.fixed_range) {
        Some((lo, hi)) if hi > lo => {
            let ticks = (0..=4)
                .map(|k| lo + (hi - lo) * (k as f64 / 4.0))
                .collect();
            (lo, hi, ticks)
        }
        _ => {
            // 多条序列共享纵轴时，取最严格的（最大）下限
            let floor = series.iter().filter_map(|s| s.axis_floor).reduce(f64::max);
            axis_scale(dmin, dmax, if int_ticks { Some(0) } else { None }, floor)
        }
    };
    let span = hi - lo;

    // ---- 横轴：按时间域映射（各序列点数/时刻可不同；并纳入事件点，确保标注落在图内） ----
    let (mut t0, mut t1) = (i64::MAX, i64::MIN);
    for s in series {
        if let (Some(first), Some(last)) = (s.points.first(), s.points.last()) {
            t0 = t0.min(first.0);
            t1 = t1.max(last.0);
        }
    }
    for ev in events {
        t0 = t0.min(ev.t);
        t1 = t1.max(ev.t);
    }
    if t1 <= t0 {
        t0 -= 1;
        t1 += 1;
    }

    let plot_w = W - PAD_L - PAD_R;
    let plot_h = H - PAD_T - PAD_B;
    let x_at = |t: i64| PAD_L + plot_w * ((t - t0) as f64 / (t1 - t0) as f64);
    let y_at = |v: f64| PAD_T + plot_h * (1.0 - (v - lo) / span);

    // 横向时间网格（5 等分）作为背景，让多个时间点更易读
    let time_fracs: [f64; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];
    let time_xs: Vec<f64> = time_fracs.iter().map(|f| PAD_L + plot_w * f).collect();
    let time_ts: Vec<i64> = time_fracs
        .iter()
        .map(|f| (t0 as f64 + (t1 - t0) as f64 * f) as i64)
        .collect();
    for &x in &time_xs {
        out.push_str(&format!(
            r##"<line x1="{x:.1}" y1="{PAD_T}" x2="{x:.1}" y2="{:.1}" stroke="{GRID}" stroke-width="1" opacity="0.45"/>"##,
            PAD_T + plot_h
        ));
    }

    // 网格 + 纵轴刻度
    for val in &ticks {
        let y = y_at(*val);
        out.push_str(&format!(
            r##"<line x1="{PAD_L}" y1="{y:.1}" x2="{:.1}" y2="{y:.1}" stroke="{GRID}" stroke-width="1"/>"##,
            W - PAD_R
        ));
        out.push_str(&format!(
            r##"<text x="{:.1}" y="{:.1}" text-anchor="end" fill="{AXIS_TEXT}" font-size="10">{}</text>"##,
            PAD_L - 8.0,
            y + 3.5,
            format_tick(*val, if int_ticks { Some(0) } else { None })
        ));
    }

    // 面积 + 线（按离线缺口分段；step = 方波）
    for s in series {
        if s.points.is_empty() {
            continue;
        }
        for seg in split_segments(&s.points) {
            let (start_idx, end_idx) = seg;
            let mut area = format!(
                "M {:.1} {:.1}",
                x_at(s.points[start_idx].0),
                y_at(s.points[start_idx].1)
            );
            let mut line = area.clone();
            for i in (start_idx + 1)..=end_idx {
                let (px, py) = (x_at(s.points[i].0), y_at(s.points[i].1));
                let segs_d: Vec<String> = if s.step {
                    // 先水平延伸到本点时刻（保持上一个值），再垂直跳变到新值
                    vec![
                        format!(" L {px:.1} {:.1}", y_at(s.points[i - 1].1)),
                        format!(" L {px:.1} {py:.1}"),
                    ]
                } else {
                    vec![format!(" L {px:.1} {py:.1}")]
                };
                for d in segs_d {
                    line.push_str(&d);
                    area.push_str(&d);
                }
            }
            area.push_str(&format!(
                " L {:.1} {:.1} L {:.1} {:.1} Z",
                x_at(s.points[end_idx].0),
                PAD_T + plot_h,
                x_at(s.points[start_idx].0),
                PAD_T + plot_h
            ));
            out.push_str(&format!(
                r##"<path d="{area}" fill="{}" opacity="0.10"/>"##,
                s.color
            ));
            out.push_str(&format!(
                r##"<path d="{line}" fill="none" stroke="{}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>"##,
                s.color
            ));
        }
    }

    // 事件标注：竖向虚线 + 顶部色点 + 短时分标签（HH:MM）
    for ev in events {
        let x = x_at(ev.t);
        if x < PAD_L || x > W - PAD_R {
            continue;
        }
        let color = ev.kind.color();
        out.push_str(&format!(
            r##"<line x1="{x:.1}" y1="{PAD_T}" x2="{x:.1}" y2="{:.1}" stroke="{color}" stroke-width="1.3" stroke-dasharray="3 3" opacity="0.85"/>"##,
            PAD_T + plot_h
        ));
        out.push_str(&format!(
            r##"<circle cx="{x:.1}" cy="{PAD_T:.1}" r="3" fill="{color}"/>"##
        ));
        let mid = PAD_T + plot_h * 0.5;
        out.push_str(&format!(
            r##"<text x="{x:.1}" y="{mid:.1}" transform="rotate(-90 {x:.1} {mid:.1})" text-anchor="middle" fill="{color}" font-size="8">{}</text>"##,
            fmt_hm(ev.t)
        ));
    }

    // 时间轴标签（5 等分，含起止时间，不再只有首尾两点）
    for (i, (&x, &tt)) in time_xs.iter().zip(time_ts.iter()).enumerate() {
        let anchor = match i {
            0 => "start",
            4 => "end",
            _ => "middle",
        };
        out.push_str(&format!(
            r##"<text x="{x:.1}" y="{}" text-anchor="{anchor}" fill="{AXIS_TEXT}" font-size="10">{}</text>"##,
            H - 8.0,
            fmt_time(tt)
        ));
    }

    // 图例：色块 + 名称（单位），从左往右排布
    let mut lx = PAD_L;
    for s in series {
        out.push_str(&format!(
            r##"<rect x="{lx:.1}" y="4" width="9" height="9" rx="2" fill="{}"/>"##,
            s.color
        ));
        let text = format!("{} ({})", s.label, s.unit);
        out.push_str(&format!(
            r##"<text x="{:.1}" y="12" fill="{LEGEND_TEXT}" font-size="11">{}</text>"##,
            lx + 13.0,
            escape(&text)
        ));
        lx += 13.0 + text.chars().count() as f64 * 6.6 + 16.0;
    }
    out.push_str("</svg>");
    out
}

pub fn fmt_time(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// 短时分（HH:MM），用于事件标注的纵向标签。
pub fn fmt_hm(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// 按时间缺口把点列切成连续段，返回 `(起点下标, 终点下标)`。
/// 缺口阈值 = 相邻间隔中位数的 2 倍（至少 60s）：断线让离线时段不被画成假连续曲线。
fn split_segments(points: &[(i64, f64)]) -> Vec<(usize, usize)> {
    if points.len() < 2 {
        return if points.is_empty() {
            Vec::new()
        } else {
            vec![(0, 0)]
        };
    }
    let mut diffs: Vec<i64> = points.windows(2).map(|w| w[1].0 - w[0].0).collect();
    diffs.sort_unstable();
    let median = diffs[diffs.len() / 2].max(1);
    let threshold = (median * 2).max(60_000);

    let mut segs = Vec::new();
    let mut start = 0usize;
    for i in 1..points.len() {
        if points[i].0 - points[i - 1].0 > threshold {
            segs.push((start, i - 1));
            start = i;
        }
    }
    segs.push((start, points.len() - 1));
    segs
}

/// 纵轴范围与刻度值。
/// * `tick_decimals == Some(0)`：整数刻度——步长取整（≥1），范围对齐到 4 等分；
/// * `floor` 有值：下界**强制**为该值（电压恒从 0 起），上界吸附到整步长，
///   避免出现 16.35 这种别扭的顶刻度；
/// * 其他：数据范围外扩 12%，均分 5 档（保持原行为）。
fn axis_scale(
    min: f64,
    max: f64,
    tick_decimals: Option<u32>,
    floor: Option<f64>,
) -> (f64, f64, Vec<f64>) {
    if tick_decimals == Some(0) {
        let (lo, hi, step) = integer_axis(min, max);
        let mut ticks = Vec::new();
        let mut v = lo;
        while v <= hi + step * 0.5 {
            ticks.push(v);
            v += step;
        }
        return (lo, hi, ticks);
    }
    let pad = (max - min) * 0.12;
    let mut lo = min - pad;
    let mut hi = max + pad;
    if let Some(f) = floor {
        lo = f;
        if hi <= lo {
            hi = lo + 1.0;
        }
        // 顶刻度吸附到整步长：约 4 等分，实际 4~5 格
        let step = nice_step((hi - lo) / 4.0);
        let n = ((hi - lo) / step).ceil().max(1.0);
        hi = lo + step * n;
        // 自下而上生成，保证下界是精确的 0（避免 -0 这种刻度文案）
        let mut ticks: Vec<f64> = (0..=n as usize).map(|k| lo + step * k as f64).collect();
        ticks.reverse();
        return (lo, hi, ticks);
    }
    if hi <= lo {
        hi = lo + 1.0;
    }
    let span = hi - lo;
    let ticks = (0..=4).map(|k| hi - span * (k as f64 / 4.0)).collect();
    (lo, hi, ticks)
}

/// 把步长吸附到 1 / 1.2 / 1.5 / 2 / 2.5 / 3 / 4 / 5 / 6 / 8 / 10 × 10^n，
/// 让刻度落在整数或半整数上。档位偏密是为了避免 58 这种跨度直接跳到 100
/// （220V 市电只剩 3 格），保证纵轴始终有 4~5 条网格线。
fn nice_step(raw: f64) -> f64 {
    if !(raw > 0.0) || !raw.is_finite() {
        return 1.0;
    }
    let exp = raw.log10().floor();
    let base = 10f64.powf(exp);
    let n = raw / base; // [1, 10)
    let m = if n <= 1.0 {
        1.0
    } else if n <= 1.2 {
        1.2
    } else if n <= 1.5 {
        1.5
    } else if n <= 2.0 {
        2.0
    } else if n <= 2.5 {
        2.5
    } else if n <= 3.0 {
        3.0
    } else if n <= 4.0 {
        4.0
    } else if n <= 5.0 {
        5.0
    } else if n <= 6.0 {
        6.0
    } else if n <= 8.0 {
        8.0
    } else {
        10.0
    };
    m * base
}

/// 整数刻度：步长 = ceil(跨度/4) 且 ≥1；把 [floor(min), ceil(max)] 向外扩到
/// 至少 4 个步长，保证约 5 条网格线。例：61.0~62.0 → 60..64（步长 1）。
fn integer_axis(min: f64, max: f64) -> (f64, f64, f64) {
    let span = (max - min).max(1.0);
    let step = (span / 4.0).ceil().max(1.0);
    let mut lo = min.floor();
    let mut hi = max.ceil();
    let want = step * 4.0;
    if hi - lo < want {
        let pad = want - (hi - lo);
        let lo_pad = (pad / 2.0).floor();
        lo -= lo_pad;
        hi += pad - lo_pad;
    }
    (lo, hi, step)
}

fn format_tick(v: f64, tick_decimals: Option<u32>) -> String {
    match tick_decimals {
        Some(0) => format!("{v:.0}"),
        _ => format_value(v),
    }
}

pub fn format_value(v: f64) -> String {
    if v.abs() >= 100.0 || (v - v.round()).abs() < 1e-9 {
        // 大数或整数值：不显示多余小数（0 / 5 / 10 而不是 0.0 / 5.0 / 10.0）
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(label: &str, unit: &str, color: &str, points: Vec<(i64, f64)>) -> Series {
        Series {
            label: label.into(),
            unit: unit.into(),
            color: color.into(),
            points,
            tick_decimals: None,
            step: false,
            fixed_range: None,
            axis_floor: None,
        }
    }

    #[test]
    fn segments_break_on_time_gaps() {
        let pts: Vec<(i64, f64)> = (0..4)
            .map(|i| (i * 60_000, 1.0))
            .chain([(3_600_000, 1.0), (3_060_000, 1.0), (3_120_000, 1.0)])
            .collect();
        let segs = split_segments(&pts);
        assert_eq!(segs, vec![(0, 3), (4, 6)]);
    }

    #[test]
    fn segments_single_for_continuous() {
        let pts: Vec<(i64, f64)> = (0..5).map(|i| (i * 300_000, 2.0)).collect();
        assert_eq!(split_segments(&pts), vec![(0, 4)]);
    }

    #[test]
    fn integer_axis_uses_whole_degrees() {
        // 61.0~61.9 的窄带也要给出整度刻度：60,61,62,63,64
        let (lo, hi, ticks) = axis_scale(61.02, 61.98, Some(0), None);
        assert_eq!((lo, hi), (60.0, 64.0));
        assert_eq!(ticks, vec![60.0, 61.0, 62.0, 63.0, 64.0]);
    }

    #[test]
    fn integer_axis_scales_up_for_wide_range() {
        // 55.3~64.8 → 步长 3，范围对齐 4 等分
        let (lo, hi, ticks) = axis_scale(55.3, 64.8, Some(0), None);
        assert_eq!(ticks.first(), Some(&54.0));
        assert_eq!(ticks.last(), Some(&66.0));
        assert!(ticks.iter().all(|t| t.fract() == 0.0));
        assert!(ticks.len() >= 5);
        assert!(lo <= 55.3 && hi >= 64.8);
    }

    #[test]
    fn auto_axis_keeps_decimals_for_fractional_data() {
        let (lo, hi, ticks) = axis_scale(10.30, 10.46, None, None);
        assert_eq!(ticks.len(), 5);
        assert!(lo < 10.30 && hi > 10.46);
    }

    #[test]
    fn axis_floor_clamps_negative_voltage() {
        // 电压数据含 0 时，自动外扩会下探到负值；floor=0 应把下界钳到 0，
        // 且上界吸附到整步长（16.35 → 20），刻度 0/5/10/15/20。
        let (lo, hi, ticks) = axis_scale(0.0, 14.6, None, Some(0.0));
        assert_eq!(lo, 0.0);
        assert!(ticks.iter().all(|t| *t >= 0.0), "不应出现负刻度: {ticks:?}");
        assert_eq!(ticks.last(), Some(&0.0));
        assert_eq!(hi, 20.0);
        assert_eq!(ticks, vec![20.0, 15.0, 10.0, 5.0, 0.0]);
    }

    #[test]
    fn axis_floor_forces_zero_base_for_high_voltage() {
        // 电压图纵轴一律从 0 起：210~230 的市电数据也要给出 0 刻度，
        // 且顶刻度吸附到整步长（232.4 → 240），网格 4~5 格。
        let (lo, hi, ticks) = axis_scale(210.0, 230.0, None, Some(0.0));
        assert_eq!(lo, 0.0);
        assert_eq!(ticks.last(), Some(&0.0), "最低刻度必须是 0: {ticks:?}");
        assert!(hi >= 230.0, "顶刻度不能压住数据: {hi}");
        assert!(
            (4..=6).contains(&ticks.len()),
            "刻度数量应保持在 4~6 条: {ticks:?}"
        );
        assert!(
            ticks.windows(2).all(|w| w[0] > w[1]),
            "刻度应自顶向下递减: {ticks:?}"
        );
    }

    #[test]
    fn axis_floor_keeps_zero_exact_without_negative_zero_label() {
        // 浮点误差不能把最低刻度算成 -0.0000001（会被格式化成 "-0"）
        let (lo, _, ticks) = axis_scale(11.8, 12.6, None, Some(0.0));
        assert_eq!(lo, 0.0);
        assert_eq!(ticks.last().copied(), Some(0.0));
        assert!(
            ticks.iter().all(|t| t.is_sign_positive()),
            "不应出现 -0 刻度: {ticks:?}"
        );
    }

    #[test]
    fn integral_ticks_drop_trailing_zero() {
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(5.0), "5");
        assert_eq!(format_value(10.1), "10.1");
    }

    #[test]
    fn fixed_range_pins_axis() {
        // 电量固定 0~100：即使数据只有 63~84，刻度也应是 0/25/50/75/100
        let mut s = series("电量", "%", "#fff", vec![(0, 63.0), (60_000, 84.0)]);
        s.fixed_range = Some((0.0, 100.0));
        s.tick_decimals = Some(0); // 电量为整数百分比
        let svg = line_chart(&s, "空");
        for t in ["0", "25", "50", "75", "100"] {
            assert!(svg.contains(&format!(">{t}<")), "缺少固定刻度 {t}");
        }
        assert!(!svg.contains(">60<"), "不应按数据范围缩放: {svg}");
    }

    #[test]
    fn multi_series_shares_axis_and_legend() {
        let a = series("电源电压", "V", "#059669", vec![(0, 10.1), (60_000, 10.4)]);
        let b = series("UPS电压", "V", "#1d4ed8", vec![(0, 13.0), (60_000, 14.6)]);
        let svg = line_chart_multi(&[a, b], &[], "空");
        // 两条线、两个图例色块；两条序列标签齐全
        assert_eq!(svg.matches("fill=\"none\"").count(), 2);
        assert_eq!(svg.matches(r#"width="9""#).count(), 2);
        assert!(svg.contains("电源电压 (V)"));
        assert!(svg.contains("UPS电压 (V)"));
        // 纵轴刻度覆盖两组数据的并集（最大 14.6）
        let max_tick: f64 = svg
            .split("font-size=\"10\">")
            .skip(1)
            .filter_map(|seg| seg.split('<').next()?.parse::<f64>().ok())
            .fold(f64::MIN, f64::max);
        assert!(max_tick >= 14.6, "纵轴应覆盖 14.6，实际最大刻度 {max_tick}");
    }

    #[test]
    fn multi_series_handles_different_lengths() {
        // 两条序列点数不同也能共用时间轴（x 按时间映射而非下标）
        let a = series("A", "V", "#fff", vec![(0, 1.0), (60_000, 2.0), (120_000, 3.0)]);
        let b = series("B", "V", "#f00", vec![(30_000, 5.0), (90_000, 6.0)]);
        let svg = line_chart_multi(&[a, b], &[], "空");
        assert_eq!(svg.matches("fill=\"none\"").count(), 2);
        assert!(svg.contains("A (V)") && svg.contains("B (V)"));
        // 纵轴覆盖两条序列的并集（最大 6.0）
        let max_tick: f64 = svg
            .split("font-size=\"10\">")
            .skip(1)
            .filter_map(|seg| seg.split('<').next()?.parse::<f64>().ok())
            .fold(f64::MIN, f64::max);
        assert!(max_tick >= 6.0, "纵轴应覆盖 6.0，实际最大刻度 {max_tick}");
    }

    #[test]
    fn events_are_marked_on_chart() {
        // 电量曲线叠加「充电开始」「充满」两个事件：应有虚线 + 按类型着色
        let mut s = series("电量", "%", "#fff", vec![(0, 63.0), (3_600_000, 84.0), (7_200_000, 100.0)]);
        s.fixed_range = Some((0.0, 100.0));
        s.tick_decimals = Some(0);
        let events = vec![
            ChartEvent { t: 3_600_000, kind: EventKind::Charge },
            ChartEvent { t: 7_200_000, kind: EventKind::Full },
        ];
        let svg = line_chart_events(&s, &events, "空");
        assert!(svg.contains("stroke-dasharray=\"3 3\""), "应绘制事件虚线");
        assert!(svg.contains("stroke=\"#059669\""), "充电事件应为绿色");
        assert!(svg.contains("stroke=\"#1d4ed8\""), "充满事件应为蓝色");
        // 时间轴应有 5 个等分标签（不再只有首尾）
        assert!(svg.matches("text-anchor=\"middle\"").count() >= 3);
    }

    #[test]
    fn step_chart_path_holds_value() {
        let s = Series {
            label: "温度".into(),
            unit: "℃".into(),
            color: "#f5a524".into(),
            points: vec![(0, 60.0), (300_000, 62.0), (600_000, 61.0)],
            tick_decimals: Some(0),
            step: true,
            fixed_range: None,
            axis_floor: None,
        };
        let svg = line_chart(&s, "暂无数据");
        let line = line_path(&svg);
        // 方波：每个中间点贡献「水平 + 垂直」两段，成对的两段 x 相同
        let segs: Vec<(f64, f64)> = line
            .split(" L ")
            .skip(1)
            .map(|s| {
                let mut it = s.split(' ');
                (
                    it.next().unwrap().parse().unwrap(),
                    it.next().unwrap().parse().unwrap(),
                )
            })
            .collect();
        assert_eq!(segs.len(), 4, "两个中间点应各产生两段: {segs:?}");
        assert_eq!(segs[0].0, segs[1].0, "水平段与垂直段 x 应相同");
        assert_eq!(segs[2].0, segs[3].0);
        assert_ne!(segs[0].1, segs[1].1, "同 x 上应有两次不同 y（跳变）");

        // 整度刻度：59..63 全整数，无 60.5 / 62.1 之类
        for t in ["59", "60", "61", "62", "63"] {
            assert!(svg.contains(&format!(">{t}<")), "缺少整度刻度 {t}");
        }
        assert!(!svg.contains(".5</text>"), "不应出现小数刻度");
    }

    #[test]
    fn chart_renders_without_points() {
        let s = series("电量", "%", "#fff", Vec::new());
        let svg = line_chart(&s, "暂无数据");
        assert!(svg.contains("暂无数据"));
        // 多序列里只有一条有点也要正常画
        let a = series("A", "V", "#fff", vec![(0, 1.0), (60_000, 2.0)]);
        let b = series("B", "V", "#f00", Vec::new());
        let svg = line_chart_multi(&[a, b], &[], "空");
        assert_eq!(svg.matches("fill=\"none\"").count(), 1);
    }

    fn line_path(svg: &str) -> String {
        svg.split("fill=\"none\"")
            .next()
            .unwrap_or("")
            .rsplit("<path d=\"")
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches('"')
            .to_string()
    }
}
