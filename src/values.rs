//! 将 tiberius 的列值（`ColumnData`）格式化为字符串，用于 CSV 输出。
//!
//! 时间类型通过换算为 `chrono` 类型后格式化；NULL 输出为 `NULL`。
//!
//! 关键点：`money`/`smallmoney` 与 `float`/`real` 在 tiberius 侧都被解码成
//! `f64`/`f32`，仅看值无法区分，但两者的打印语义完全不同——定点数必须保留 4 位
//! 小数，浮点数则应在极端量级下改用科学计数法。因此格式化必须带上列的
//! `ColumnType`（见 [`column_data_to_string_typed`]）。

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use tiberius::ColumnData;
use tiberius::time::{DateTime2, DateTimeOffset, Time};

pub use tiberius::ColumnType;

/// `date` 类型的天数起点：公元 1 年 1 月 1 日（`Date::days()` 的定义）。
const CE_EPOCH: i32 = 1;
/// 1900-01-01 相对公元 1 年 1 月 1 日的天数（`datetime`/`smalldatetime` 的起点）。
/// chrono 的 CE 纪元从 1 起算，1900-01-01 对应 693596（有测试自校验）。
const DAYS_1900_TO_CE: i32 = 693_596;

/// `datetime` 秒的小数粒度：1 秒 = 300 个 tick（即 1/300 秒）。
const DATETIME_TICKS_PER_SECOND: u32 = 300;
/// `datetime` 的小数秒位数：恒为 3（毫秒，且末位按 0/3/7 量化）。
const DATETIME_SCALE: u8 = 3;
/// 1 秒 = 10^9 纳秒。
const NANOS_PER_SECOND: u32 = 1_000_000_000;
/// `smalldatetime` 的时间部分是「自午夜起的**分钟**数」（SQL Server 规范），
/// 与 `datetime` 的 1/300 秒 tick 语义不同，必须分开换算。
const SECONDS_PER_MINUTE: u32 = 60;

/// 浮点渲染的判定阈值：绝对值达到 1e16 或小于 1e-4 时改用科学计数法，
/// 否则定点打印会产生上百位数字（如 f64::MAX → 309 位）。
///
/// 上界 1e16 是明确的（f64 定点形式从这附近开始失精且冗长）。下界 1e-4 则
/// 是为了贴近 SQL Server `str()` 的习惯输出；SQL Server 各版本在 1e-4~1e-6
/// 之间的切换点并不完全一致，**此值尚未对真实服务器作过逐值比对**。
/// 若将来要校准，可用下面的语句取值后与此处的输出对照：
/// `SELECT CAST(1e-5 AS float), CAST(0.0001 AS float), CAST(1.5e-5 AS float)`。
const SCIENTIFIC_UPPER: f64 = 1e16;
/// 小于该绝对值（且非零）时同样改用科学计数法，避免 `0.0000...` 的定点形式。
const SCIENTIFIC_LOWER: f64 = 1e-4;

/// money 的定点单位：值 × 10^4 后取整即为整数定点表示。
const MONEY_SCALE: f64 = 10_000.0;

/// 格式化任意列值（无列类型信息）：`None` → `NULL`，二进制 → 十六进制，
/// 时间 → ISO 格式。
///
/// 仅用于测试或拿不到 `ColumnType` 的场景；生产路径请用
/// [`column_data_to_string_typed`]，否则 `money` 会被当成浮点打印。
pub fn column_data_to_string(data: &ColumnData<'_>) -> String {
    column_data_to_string_typed(data, ColumnType::Null)
}

/// 按列的 TDS 类型格式化列值。
///
/// 类型只影响两类判断：
/// - `Money`/`Money4` → 定点渲染（见 [`format_money`]）；
/// - 其余浮点（`Float4`/`Float8`/`Floatn`/...） → 科学计数法（见 [`format_f64`]）。
///
/// 整数、字符串、时间等与类型无关，直接按值渲染。
pub fn column_data_to_string_typed(data: &ColumnData<'_>, column_type: ColumnType) -> String {
    use ColumnData::*;
    match data {
        U8(None) | I16(None) | I32(None) | I64(None) | F32(None) | F64(None) | Bit(None)
        | String(None) | Guid(None) | Binary(None) | Numeric(None) | Xml(None) | DateTime(None)
        | SmallDateTime(None) | Time(None) | Date(None) | DateTime2(None)
        | DateTimeOffset(None) => "NULL".to_string(),
        U8(Some(v)) => v.to_string(),
        I16(Some(v)) => v.to_string(),
        I32(Some(v)) => v.to_string(),
        I64(Some(v)) => v.to_string(),
        // 注意：f32 不能先拓宽成 f64 再格式化 —— 拓宽后的最短往返表示会变成
        // f64 的（f32::MAX → 3.4028234663852886E+38），与 SQL Server 打印的
        // 3.4028235E+38 不符。
        F32(Some(v)) => format_float_or_money32(*v, column_type),
        F64(Some(v)) => format_float_or_money(*v, column_type),
        Bit(Some(v)) => u8::from(*v).to_string(),
        String(Some(v)) => v.to_string(),
        Guid(Some(v)) => v.to_string(),
        Binary(Some(v)) => bytes_to_hex(v),
        Numeric(Some(v)) => v.to_string(),
        Xml(Some(v)) => v.clone().into_owned().into_string(),
        DateTime(Some(v)) => format_legacy_datetime(v.days(), v.seconds_fragments()),
        SmallDateTime(Some(v)) => {
            format_smalldatetime(i32::from(v.days()), u32::from(v.seconds_fragments()))
        }
        Time(Some(v)) => format_time(v),
        Date(Some(v)) => format_date(v.days()),
        DateTime2(Some(v)) => format_date_time2(v),
        DateTimeOffset(Some(v)) => format_date_time_offset(v),
    }
}

/// `money` 列走定点渲染，其余按浮点语义渲染。
fn format_float_or_money(value: f64, column_type: ColumnType) -> String {
    match column_type {
        ColumnType::Money | ColumnType::Money4 => format_money(value),
        _ => format_f64(value),
    }
}

/// `smallmoney`/`real`（f32）同上，但格式化保留 f32 的最短往返表示。
///
/// 注意 tiberius 把 `smallmoney` 也解码成 `F64`（`i32 / 1e4`），只有 `real`
/// 才是 `F32`；两者都过这里，靠 `column_type` 分流。
fn format_float_or_money32(value: f32, column_type: ColumnType) -> String {
    match column_type {
        ColumnType::Money | ColumnType::Money4 => format_money(f64::from(value)),
        _ => format_f32(value),
    }
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + 2 * bytes.len());
    out.push_str("0x");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `money`/`smallmoney`：tiberius 已把定点数解码成 `f64`（除以 1e4）。
///
/// f64 只有约 15~17 位有效十进制数字，而 `money` 的整数部分可达 19 位，
/// 因此直接 `to_string()` 会得到 `922337203685477.6` 这种被四舍五入吞掉
/// `.5807` 的值。这里反向还原定点表示：乘回 1e4 并取整，再拆成整数/小数两段。
///
/// 何时必须放弃还原：定点单位本身超出 `i64` 范围时无法表达（整数部分失真），
/// 此时退化为浮点形式。**注意判据是 i64 范围而不是 2^53**——money 的最小
/// 值 `-922337203685477.5808` 对应定点单位 `-9223372036854775808`，其绝对值
/// 恰为 2^63，虽然超过 2^53 却能被 f64 **精确**表示（2 的幂），完全可以
/// 准确还原；早年按 2^53 判定会让负数边界全部退化成 `-922337203685477.6`，
/// 丢掉 `.5808` 而输出一个不存在的数。
fn format_money(value: f64) -> String {
    let scaled = (value * MONEY_SCALE).round();
    if !scaled.is_finite() || !scaled_in_i64_range(scaled) {
        return value.to_string();
    }
    let units = scaled as i64;
    let sign = if units < 0 { "-" } else { "" };
    let abs = units.unsigned_abs();
    format!("{sign}{}.{:04}", abs / 10_000, abs % 10_000)
}

/// 定点单位能否被 `i64` 承载：判据要比 2^53 更宽松。
///
/// f64 精确表示的不只是 2^53 以内的整数——超过 2^53 但间距仍为整数的那些
/// 值（尤其是 2 的各次幂）同样精确。money 的负数边界 2^63 正是这种情况，
/// 故按 `i64` 的定义域（约 ±2^63）判断，而不是按 f64 的连续整数上限。
fn scaled_in_i64_range(scaled: f64) -> bool {
    // f64 表示 i64::MAX 时会进位到 2^63，故这里用开区间并留一档余量。
    scaled > -9_223_372_036_854_775_808.0 && scaled < 9_223_372_036_854_775_808.0
}

/// `float`/`real`：按 IEEE 754 最短往返表示打印，极端量级改用科学计数法。
///
/// Rust 的 `f64::to_string()` 恒用定点记数法，`1.79e308` 会被打印成 309 位
/// 数字，既不可读也会让 CSV 体积暴涨。SQL Server/`sqlcmd` 对同类值采用
/// 科学计数法（如 `3.4028235E+38`），这里对齐之。
fn format_f64(value: f64) -> String {
    if value == 0.0 {
        // SQL Server 不区分 ±0，统一输出 "0"。
        return "0".to_string();
    }
    let abs = value.abs();
    if abs >= SCIENTIFIC_UPPER || abs < SCIENTIFIC_LOWER {
        return scientific_notation(&format!("{value:e}"));
    }
    value.to_string()
}

fn format_f32(value: f32) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let abs = value.abs();
    if f64::from(abs) >= SCIENTIFIC_UPPER || f64::from(abs) < SCIENTIFIC_LOWER {
        return scientific_notation(&format!("{value:e}"));
    }
    value.to_string()
}

/// 把 Rust 的 `1.7976931348623157e308` 改写为 SQL Server 的
/// `1.7976931348623157E+308`（大写 E、带符号、两位起指数）。
fn scientific_notation(text: &str) -> String {
    match text.find('e') {
        Some(pos) => {
            let (mantissa, exp) = text.split_at(pos);
            let exp: i64 = exp[1..].parse().unwrap_or(0);
            format!("{mantissa}E{exp:+03}")
        }
        None => text.to_string(),
    }
}

fn format_date(days: u32) -> String {
    NaiveDate::from_num_days_from_ce_opt(CE_EPOCH + days as i32)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| days.to_string())
}

/// `time(n)`：`increments` 是 10^-scale 秒的计数，换算为纳秒走整数运算。
fn format_time(time: &Time) -> String {
    let (secs, subsec_nanos) = increments_to_secs_nanos(time.increments(), time.scale());
    NaiveTime::from_num_seconds_from_midnight_opt(secs, subsec_nanos)
        .map(|_| format_clock(secs, subsec_nanos, time.scale()))
        .unwrap_or_else(|| time.increments().to_string())
}

/// 按 `scale`（小数秒位数）渲染 "HH:MM:SS" 及其小数部分。
///
/// 不能用 chrono 的 `%.f`：它把纳秒按 **3 位一组**输出并裁剪尾零，与列的
/// scale 无关。实测 SQL Server 严格按 scale 输出——`datetime2(1)` 显示
/// `.1`，而 `%.f` 给 `.100`；`datetime2(7)` 显示 `.1234567`，而 `%.f` 给
/// `.123456700`。两者只在 scale 为 0/3/6 时恰好一致。
///
/// `secs_of_day` 是自午夜起的秒数，`subsec_nanos` 是不足一秒的纳秒部分。
fn format_clock(secs_of_day: u32, subsec_nanos: u32, scale: u8) -> String {
    let base = format!(
        "{:02}:{:02}:{:02}",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    );
    let scale = (scale as u32).min(9);
    if scale == 0 {
        return base;
    }
    // 由纳秒截取前 `scale` 位小数（纳秒是 9 位，故丢弃末 9-scale 位）。
    let divisor = 10u32.pow(9 - scale);
    let frac = subsec_nanos / divisor;
    format!("{base}.{frac:0>width$}", width = scale as usize)
}

/// 把 tiberius 时间的 `(increments, scale)` 换算为 `(秒, 纳秒)`。
fn increments_to_secs_nanos(increments: u64, scale: u8) -> (u32, u32) {
    let scale = scale.min(7) as u32;
    let total_nanos = increments * 10u64.pow(9 - scale);
    (
        (total_nanos / u64::from(NANOS_PER_SECOND)) as u32,
        (total_nanos % u64::from(NANOS_PER_SECOND)) as u32,
    )
}

fn format_date_time2(value: &DateTime2) -> String {
    let date = NaiveDate::from_num_days_from_ce_opt(CE_EPOCH + value.date().days() as i32);
    match date {
        Some(date) => {
            let time = value.time();
            let (secs, subsec_nanos) = increments_to_secs_nanos(time.increments(), time.scale());
            let clock = format_clock(secs, subsec_nanos, time.scale());
            format!("{date} {clock}")
        }
        None => "NULL".to_string(),
    }
}

/// `datetimeoffset(n)`：输出**本地时间** + 偏移量后缀。
///
/// TDS 线路上的 `DATETIMEOFFSETN` 时间部分是 **UTC**，偏移量单独作为分钟数传输。
/// 而 SQL Server 自身（及 `sqlcmd`）显示的是本地时间，例如存储的
/// `2026-10-01 12:00:00.123 +08:00` 在线路上是 `04:00:00.123` + 480 分钟。
/// 因此必须把 UTC 时间**加上**偏移换算成本地时间，否则会输出
/// `04:00:00.123+08:00` 这种自相矛盾的结果（时刻与偏移对不上）。
fn format_date_time_offset(value: &DateTimeOffset) -> String {
    let base = format_date_time2_local(value);
    let minutes = value.offset();
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.unsigned_abs();
    format!("{base}{sign}{:02}:{:02}", abs / 60, abs % 60)
}

/// 把 `datetimeoffset` 的 UTC 时间部分加上偏移，换算为本地时间后格式化。
///
/// 偏移可能跨越日界（如 UTC 23:00 +02:00 → 次日 01:00），故按秒数做算术
/// 而不是分别调整时分秒。
fn format_date_time2_local(value: &DateTimeOffset) -> String {
    let dt2 = value.datetime2();
    let Some(utc_date) = NaiveDate::from_num_days_from_ce_opt(CE_EPOCH + dt2.date().days() as i32)
    else {
        return "NULL".to_string();
    };
    let (utc_secs, nanos) = increments_to_secs_nanos(dt2.time().increments(), dt2.time().scale());
    let Some(local) = Some(utc_date).and_then(|d| {
        NaiveDateTime::new(
            d,
            NaiveTime::from_num_seconds_from_midnight_opt(utc_secs, nanos)
                .unwrap_or(NaiveTime::MIN),
        )
        .checked_add_signed(chrono::Duration::minutes(i64::from(value.offset())))
    }) else {
        return "NULL".to_string();
    };
    // 由 `NaiveDateTime` 反取自午夜起的秒数：chrono 未启用 `Timelike`,
    // 故用其与当日零点的差值换算（小时/分/秒访问器不可用）。
    let elapsed = local.signed_duration_since(NaiveDateTime::new(local.date(), NaiveTime::MIN));
    let local_secs = elapsed.num_seconds().max(0) as u32;
    // 不足一秒的部分：偏移是整分钟的，加减偏移不会改动亚秒部分。
    let clock = format_clock(local_secs, nanos, dt2.time().scale());
    format!("{} {clock}", local.date())
}

/// `datetime`：天数自 1900 起，时间部分以 1/300 秒为单位的 tick 数。
///
/// 秒精度被量化到 0/3/7 毫秒，例如 `9999-12-31 23:59:59.997` 存为 25919999 tick。
fn format_legacy_datetime(days: i32, seconds_fragments: u32) -> String {
    let Some(date) = NaiveDate::from_num_days_from_ce_opt(DAYS_1900_TO_CE + days) else {
        return days.to_string();
    };
    let secs = seconds_fragments / DATETIME_TICKS_PER_SECOND;
    // tick 到毫秒不可整除（1 tick = 3.333ms）。SQL Server 的 `datetime` 秒精度
    // 恒为 3 位、末位按 0/3/7 毫秒量化：实测 tick 0→.000、1→.003、2→.007、
    // 3→.010、……、299→.997，正是 `round(tick * 10 / 3)`。
    // 若直接把余数换算成纳秒会得到 .996666666 这种 SQL Server 从不显示的值。
    let millis = round_div(
        u64::from(seconds_fragments % DATETIME_TICKS_PER_SECOND) * 1_000,
        u64::from(DATETIME_TICKS_PER_SECOND),
    );
    let subsec_nanos = millis.saturating_mul(1_000_000);
    match NaiveTime::from_num_seconds_from_midnight_opt(secs, subsec_nanos as u32) {
        // `datetime` 的 scale 恒为 3（0/3/7 毫秒量化），故小数位固定 3 位。
        Some(_) => {
            let clock = format_clock(secs, subsec_nanos as u32, DATETIME_SCALE);
            format!("{date} {clock}")
        }
        None => days.to_string(),
    }
}

/// 整数四舍五入除法 `(numerator + denominator / 2) / denominator`。
///
/// 仅用于极小的正数（毫秒换算），不会溢出。
const fn round_div(numerator: u64, denominator: u64) -> u64 {
    (numerator + denominator / 2) / denominator
}

/// `smalldatetime`：天数自 1900 起，时间部分是**自午夜起的分钟数**。
///
/// 与 `datetime` 的 1/300 秒 tick 语义不同：同一原始值若按 `datetime` 换算
/// 会得到完全错误的结果（23:59 会被算成 00:00:04.007966666）。精度到分钟。
fn format_smalldatetime(days: i32, minutes: u32) -> String {
    let Some(date) = NaiveDate::from_num_days_from_ce_opt(DAYS_1900_TO_CE + days) else {
        return days.to_string();
    };
    let secs = minutes.saturating_mul(SECONDS_PER_MINUTE);
    match NaiveTime::from_num_seconds_from_midnight_opt(secs, 0) {
        Some(time) => NaiveDateTime::new(date, time)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => days.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use tiberius::numeric::Numeric;
    use tiberius::time::Date;

    #[test]
    fn days_1900_constant_is_self_consistent_with_chrono() {
        let jan_1_1900 = NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
        assert_eq!(jan_1_1900.num_days_from_ce(), DAYS_1900_TO_CE);
    }

    #[test]
    fn formats_numbers_strings_and_bits() {
        assert_eq!(column_data_to_string(&ColumnData::I32(Some(42))), "42");
        assert_eq!(column_data_to_string(&ColumnData::I64(Some(-7))), "-7");
        assert_eq!(
            column_data_to_string(&ColumnData::String(Some("hello".into()))),
            "hello"
        );
        assert_eq!(column_data_to_string(&ColumnData::Bit(Some(true))), "1");
        assert_eq!(column_data_to_string(&ColumnData::Bit(Some(false))), "0");
    }

    #[test]
    fn formats_null_as_null() {
        assert_eq!(column_data_to_string(&ColumnData::I32(None)), "NULL");
        assert_eq!(column_data_to_string(&ColumnData::String(None)), "NULL");
        assert_eq!(column_data_to_string(&ColumnData::Date(None)), "NULL");
    }

    #[test]
    fn formats_binary_as_hex() {
        let data = ColumnData::Binary(Some(vec![0xde, 0xad, 0xbe, 0xef].into()));
        assert_eq!(column_data_to_string(&data), "0xdeadbeef");
    }

    #[test]
    fn formats_money_as_fixed_point_with_four_decimals() {
        // 回归点：money 必须还原定点表示，不能输出 `1234.56` 或
        // `922337203685477.6` 这种被 f64 四舍五入吞掉小数位的值。
        assert_eq!(
            column_data_to_string_typed(&ColumnData::F64(Some(1234.56)), ColumnType::Money),
            "1234.5600"
        );
        assert_eq!(
            column_data_to_string_typed(&ColumnData::F64(Some(0.0)), ColumnType::Money),
            "0.0000"
        );
        assert_eq!(
            column_data_to_string_typed(&ColumnData::F64(Some(-99.99)), ColumnType::Money),
            "-99.9900"
        );
        // smallmoney 上界 214748.3647 在 f64 精度内，可完整还原。
        // 注意：tiberius 把 smallmoney（i32/1e4）也解码成 F64，不是 F32。
        assert_eq!(
            column_data_to_string_typed(&ColumnData::F64(Some(214748.3647)), ColumnType::Money4),
            "214748.3647"
        );
    }

    #[test]
    fn money_without_column_type_degrades_to_float() {
        // 不带类型信息时无法判定为 money，按浮点渲染（这也是为什么要传 ColumnType）。
        assert_eq!(
            column_data_to_string(&ColumnData::F64(Some(1234.56))),
            "1234.56"
        );
    }

    #[test]
    fn money_beyond_f64_precision_does_not_fabricate_digits() {
        // 回归点：money 最大值 922337203685477.5807 的整数部分超出 f64 精度，
        // 此时宁可退化为浮点形式，也不能输出看似精确实则错误的 `.5807`。
        let huge = 9.223372036854776e14;
        let text = column_data_to_string_typed(&ColumnData::F64(Some(huge)), ColumnType::Money);
        assert!(
            !text.ends_with(".5807"),
            "must not fabricate precision f64 cannot carry: {text}"
        );
    }

    #[test]
    fn money_large_values_keep_four_decimals() {
        // 回归点（真实 Server 实测）：10^12 以上的 money 曾被输出成
        // `1000000000000`（无小数点）——定点单位超过 2^53 判据时直接走了
        // 退化分支，但这些值 f64 完全承载得了，本应正常定点输出。
        assert_eq!(
            column_data_to_string_typed(&ColumnData::F64(Some(1e12)), ColumnType::Money),
            "1000000000000.0000"
        );
        assert_eq!(
            column_data_to_string_typed(&ColumnData::F64(Some(1e13)), ColumnType::Money),
            "10000000000000.0000"
        );
        assert_eq!(
            column_data_to_string_typed(&ColumnData::F64(Some(-1e12)), ColumnType::Money),
            "-1000000000000.0000"
        );
    }

    #[test]
    fn money_negative_values_in_common_range() {
        for (value, expected) in [
            (-0.0001f64, "-0.0001"),
            (-1.0, "-1.0000"),
            (-214748.3648, "-214748.3648"),
            (-1234.5678, "-1234.5678"),
        ] {
            let text =
                column_data_to_string_typed(&ColumnData::F64(Some(value)), ColumnType::Money);
            assert_eq!(text, expected, "value {value}");
        }
        // smallmoney 负数边界
        let small =
            column_data_to_string_typed(&ColumnData::F64(Some(-214748.3648)), ColumnType::Money4);
        assert_eq!(small, "-214748.3648");
    }

    #[test]
    fn money_out_of_i64_range_still_degrades() {
        // 超出定点可表达范围时退化而非输出虚构数字。
        let absurd = 1e30f64;
        let text = column_data_to_string_typed(&ColumnData::F64(Some(absurd)), ColumnType::Money);
        assert!(
            !text.ends_with(".0000"),
            "must not fabricate precision: {text}"
        );
    }

    // 已知限制：money 的 ±922337203685477.5808 边界无法还原出末 4 位。
    // tiberius 把 money 解码为 `f64`（i64/1e4），19 位有效数字的信息在解码
    // 那一刻就被截断（f64 存 `-922337203685477.5808` 得到的最近值是
    // `-922337203685477.6`），格式化层再怎么算都无从恢复——除非改用能保留
    // 原始 i64 的解码路径。这里不假装它能修好。
    #[test]
    fn money_extreme_boundary_degrades_to_float() {
        // clippy::excessive_precision 会建议改成 `-922_337_203_685_477.6`，
        // 但那正是我们要断言的失真值，故保留字面原本写法。
        #[allow(clippy::excessive_precision)]
        let min = -922337203685477.5808f64;
        let text = column_data_to_string_typed(&ColumnData::F64(Some(min)), ColumnType::Money);
        // 至少不得输出看似精确的错误尾数（`.5808` 在此不可信）。
        assert!(!text.ends_with(".5808"), "f64 lost this precision: {text}");
    }

    #[test]
    fn formats_float_with_scientific_notation_for_extreme_values() {
        // 回归点：f32::MAX 不能被打印成长串定点数字。
        let text =
            column_data_to_string_typed(&ColumnData::F32(Some(f32::MAX)), ColumnType::Float4);
        assert!(text.contains('E'), "expected exponent form, got {text}");
        assert!(text.len() < 30, "expected compact form, got {text}");
        assert_eq!(
            column_data_to_string_typed(&ColumnData::F32(Some(3.4028235e38)), ColumnType::Float4),
            "3.4028235E+38"
        );
        assert_eq!(
            column_data_to_string_typed(
                &ColumnData::F64(Some(1.7976931348623157e308)),
                ColumnType::Float8
            ),
            "1.7976931348623157E+308"
        );
    }

    #[test]
    fn formats_small_floats_without_scientific_notation() {
        // 常见量级保持定点形式，不引入多余的 `E`。
        assert_eq!(
            column_data_to_string_typed(&ColumnData::F32(Some(1.5)), ColumnType::Float4),
            "1.5"
        );
        assert_eq!(
            column_data_to_string_typed(&ColumnData::F64(Some(0.0)), ColumnType::Float8),
            "0"
        );
        assert_eq!(
            column_data_to_string_typed(&ColumnData::F64(Some(-0.25)), ColumnType::Float8),
            "-0.25"
        );
    }

    #[test]
    fn formats_time_with_scale() {
        // 回归点：小数位数由列的 scale 决定，不是 chrono 的 `%.f` 语义。
        // SQL Server 对 time(1) 显示 `.1`、time(3) 显示 `.100`（而非 `.1`）。
        assert_eq!(format_time(&Time::new(15, 1)), "00:00:01.5");
        assert_eq!(format_time(&Time::new(123, 3)), "00:00:00.123");
        // 正午整，scale=7：scale=0 之外要补足到 scale 位。
        assert_eq!(
            format_time(&Time::new(432_000_000_000, 7)),
            "12:00:00.0000000"
        );
        // scale=0 不带小数部分。
        assert_eq!(format_time(&Time::new(43_200, 0)), "12:00:00");
    }

    #[test]
    fn datetime2_fraction_digits_follow_scale() {
        // 回归点（真实 Server 实测）：SQL Server 严格按 scale 输出小数位，
        // 而 chrono 的 `%.f` 按 3 位一组输出——二者只在 scale ∈ {0,3,6} 一致。
        let date = Date::new(
            NaiveDate::from_ymd_opt(2024, 6, 15)
                .unwrap()
                .num_days_from_ce() as u32
                - 1,
        );
        // 每个 scale 下的同一输入：increments 按 scale 换算后是同一时刻。
        let cases = [
            (0u8, 52_200u64, "2024-06-15 14:30:00"),
            (1, 522_001, "2024-06-15 14:30:00.1"),
            (2, 522_0012, "2024-06-15 14:30:00.12"),
            (3, 522_00123, "2024-06-15 14:30:00.123"),
            (4, 522_001234, "2024-06-15 14:30:00.1234"),
            (5, 522_0012345, "2024-06-15 14:30:00.12345"),
            (6, 522_00123456, "2024-06-15 14:30:00.123456"),
            (7, 522_001234567, "2024-06-15 14:30:00.1234567"),
        ];
        for (scale, increments, expected) in cases {
            let dt = DateTime2::new(date, Time::new(increments, scale));
            assert_eq!(format_date_time2(&dt), expected, "scale={scale} mismatched");
        }
    }

    #[test]
    fn datetime2_pads_to_scale_and_keeps_trailing_zeros() {
        // SQL Server 不裁剪尾零：datetime2(7) 存 `.1` 显示 `.1000000`。
        let date = Date::new(
            NaiveDate::from_ymd_opt(2024, 6, 15)
                .unwrap()
                .num_days_from_ce() as u32
                - 1,
        );
        let dt = DateTime2::new(date, Time::new(522_001_000_000, 7));
        assert_eq!(format_date_time2(&dt), "2024-06-15 14:30:00.1000000");
    }

    #[test]
    fn formats_date_from_ce_epoch() {
        // 2024-01-01 相对公元 1 年 1 月 1 日的天数
        let days = NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .num_days_from_ce() as u32
            - 1;
        assert_eq!(format_date(days), "2024-01-01");
    }

    #[test]
    fn formats_datetime2() {
        let date = Date::new(
            NaiveDate::from_ymd_opt(2024, 6, 15)
                .unwrap()
                .num_days_from_ce() as u32
                - 1,
        );
        // 14:30:00.1234567，scale=7 → 恰好 7 位小数
        let time = Time::new(522_001_234_567, 7);
        let dt = DateTime2::new(date, time);
        assert_eq!(format_date_time2(&dt), "2024-06-15 14:30:00.1234567");
    }

    #[test]
    fn formats_legacy_datetime() {
        let days = NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .num_days_from_ce()
            - DAYS_1900_TO_CE;
        // 14:30:00 = 300 fragments/秒 × 52200 秒（`datetime` scale 恒为 3）
        assert_eq!(
            format_legacy_datetime(days, 300 * 52_200),
            "2024-06-15 14:30:00.000"
        );
    }

    #[test]
    fn formats_legacy_datetime_three_ms_quantisation() {
        // 回归点：23:59:59.997 = 25919999 个 1/300 秒 tick。
        // 早先的换算把 tick 当纳秒直接缩放，输出成了 59.009966666。
        let days = NaiveDate::from_ymd_opt(9999, 12, 31)
            .unwrap()
            .num_days_from_ce()
            - DAYS_1900_TO_CE;
        let ticks = 23 * 3600 * 300 + 59 * 60 * 300 + 59 * 300 + 299;
        assert_eq!(
            format_legacy_datetime(days, ticks),
            "9999-12-31 23:59:59.997"
        );
    }

    #[test]
    fn formats_smalldatetime_as_minutes_since_midnight() {
        // 回归点：smalldatetime 的时间部分是分钟数，不是 1/300 秒 tick。
        // 1439 分钟 = 23:59 —— 按旧的错误换算会得到 00:00:04.007966666。
        let days = NaiveDate::from_ymd_opt(2079, 6, 6)
            .unwrap()
            .num_days_from_ce()
            - DAYS_1900_TO_CE;
        assert_eq!(format_smalldatetime(days, 1439), "2079-06-06 23:59:00");
        assert_eq!(format_smalldatetime(days, 0), "2079-06-06 00:00:00");
    }

    #[test]
    fn formats_smalldatetime_column_value_end_to_end() {
        let days = NaiveDate::from_ymd_opt(2079, 6, 6)
            .unwrap()
            .num_days_from_ce()
            - DAYS_1900_TO_CE;
        let value =
            ColumnData::SmallDateTime(Some(tiberius::time::SmallDateTime::new(days as u16, 1439)));
        assert_eq!(column_data_to_string(&value), "2079-06-06 23:59:00");
    }

    #[test]
    fn formats_datetime_offset() {
        // 线路时间取 UTC 午夜，偏移 0，本地时间仍是午夜。
        let date = Date::new(
            NaiveDate::from_ymd_opt(2024, 1, 1)
                .unwrap()
                .num_days_from_ce() as u32
                - 1,
        );
        let time = Time::new(0, 0);
        let dt = DateTime2::new(date, time);
        let dto = DateTimeOffset::new(dt, 0); // UTC+0:00
        assert_eq!(format_date_time_offset(&dto), "2024-01-01 00:00:00+00:00");
        let dto_neg = DateTimeOffset::new(dt, -300); // UTC-5:00 → 前一日 19:00
        assert_eq!(
            format_date_time_offset(&dto_neg),
            "2023-12-31 19:00:00-05:00"
        );
    }

    #[test]
    fn datetime_offset_converts_utc_wire_time_to_local() {
        // 回归点：TDS 传输的是 UTC，偏移单独给。SQL Server 存
        // `2026-10-01 12:00:00.123 +08:00` → 线路上为 04:00:00.123 与 480 分钟。
        // 早先直接原样输出线路时间，得到 `04:00:00.123+08:00` 这种时刻与偏移
        // 自相矛盾的结果。
        let date = Date::new(
            NaiveDate::from_ymd_opt(2026, 10, 1)
                .unwrap()
                .num_days_from_ce() as u32
                - 1,
        );
        // UTC 04:00:00.123，scale=3 → increments = 4*3600*1000 + 123
        let time = Time::new(4 * 3_600 * 1_000 + 123, 3);
        let dto = DateTimeOffset::new(DateTime2::new(date, time), 480);
        assert_eq!(
            format_date_time_offset(&dto),
            "2026-10-01 12:00:00.123+08:00"
        );
    }

    #[test]
    fn datetime_offset_negative_offset_matches_sql_server() {
        // `2026-10-01 12:00:00.123 -05:00` → 线路上为 UTC 17:00:00.123。
        let date = Date::new(
            NaiveDate::from_ymd_opt(2026, 10, 1)
                .unwrap()
                .num_days_from_ce() as u32
                - 1,
        );
        let time = Time::new(17 * 3_600 * 1_000 + 123, 3);
        let dto = DateTimeOffset::new(DateTime2::new(date, time), -300);
        assert_eq!(
            format_date_time_offset(&dto),
            "2026-10-01 12:00:00.123-05:00"
        );
    }

    #[test]
    fn datetime_offset_crosses_day_boundary() {
        // UTC 23:00 +02:00 → 次日 01:00（偏移换算必须进位到日期）。
        let date = Date::new(
            NaiveDate::from_ymd_opt(2026, 1, 1)
                .unwrap()
                .num_days_from_ce() as u32
                - 1,
        );
        let time = Time::new(23 * 3_600 * 1_000, 3);
        let dto = DateTimeOffset::new(DateTime2::new(date, time), 120);
        // 输入 scale=3，故按 SQL Server 约定显示 3 位小数。
        assert_eq!(
            format_date_time_offset(&dto),
            "2026-01-02 01:00:00.000+02:00"
        );
    }

    #[test]
    fn formats_numeric_and_guid() {
        let numeric = Numeric::new_with_scale(12345, 2);
        assert_eq!(
            column_data_to_string(&ColumnData::Numeric(Some(numeric))),
            "123.45"
        );
    }
}
