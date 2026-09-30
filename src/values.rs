//! 将 tiberius 的列值（`ColumnData`）格式化为字符串，用于 CSV 输出。
//!
//! 时间类型通过换算为 `chrono` 类型后格式化；NULL 输出为 `NULL`。

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use tiberius::time::{DateTime2, DateTimeOffset, Time};
use tiberius::ColumnData;

/// `date` 类型的天数起点：公元 1 年 1 月 1 日（`Date::days()` 的定义）。
const CE_EPOCH: i32 = 1;
/// 1900-01-01 相对公元 1 年 1 月 1 日的天数（`datetime`/`smalldatetime` 的起点）。
/// chrono 的 CE 纪元从 1 起算，1900-01-01 对应 693596（有测试自校验）。
const DAYS_1900_TO_CE: i32 = 693_596;

/// 格式化任意列值：`None` → `NULL`，二进制 → 十六进制，时间 → ISO 格式。
pub fn column_data_to_string(data: &ColumnData<'_>) -> String {
    use ColumnData::*;
    match data {
        U8(None)
        | I16(None)
        | I32(None)
        | I64(None)
        | F32(None)
        | F64(None)
        | Bit(None)
        | String(None)
        | Guid(None)
        | Binary(None)
        | Numeric(None)
        | Xml(None)
        | DateTime(None)
        | SmallDateTime(None)
        | Time(None)
        | Date(None)
        | DateTime2(None)
        | DateTimeOffset(None) => "NULL".to_string(),
        U8(Some(v)) => v.to_string(),
        I16(Some(v)) => v.to_string(),
        I32(Some(v)) => v.to_string(),
        I64(Some(v)) => v.to_string(),
        F32(Some(v)) => v.to_string(),
        F64(Some(v)) => v.to_string(),
        Bit(Some(v)) => u8::from(*v).to_string(),
        String(Some(v)) => v.to_string(),
        Guid(Some(v)) => v.to_string(),
        Binary(Some(v)) => bytes_to_hex(v),
        Numeric(Some(v)) => v.to_string(),
        Xml(Some(v)) => v.clone().into_owned().into_string(),        DateTime(Some(v)) => format_legacy_datetime(v.days(), v.seconds_fragments()),
        SmallDateTime(Some(v)) => format_legacy_datetime(i32::from(v.days()), u32::from(v.seconds_fragments())),
        Time(Some(v)) => format_time(v),
        Date(Some(v)) => format_date(v.days()),
        DateTime2(Some(v)) => format_date_time2(v),
        DateTimeOffset(Some(v)) => format_date_time_offset(v),
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

fn format_date(days: u32) -> String {
    NaiveDate::from_num_days_from_ce_opt(CE_EPOCH + days as i32)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| days.to_string())
}

/// `time(n)`：`increments` 是 10^-scale 秒的计数，换算为纳秒走整数运算。
fn format_time(time: &Time) -> String {
    let scale = time.scale().min(7) as u32;
    let total_nanos = time.increments() * 10u64.pow(9 - scale);
    let secs = (total_nanos / 1_000_000_000) as u32;
    let subsec_nanos = (total_nanos % 1_000_000_000) as u32;
    NaiveTime::from_num_seconds_from_midnight_opt(secs, subsec_nanos)
        .map(|t| t.format("%H:%M:%S%.f").to_string())
        .unwrap_or_else(|| time.increments().to_string())
}

fn format_date_time2(value: &DateTime2) -> String {
    let date = NaiveDate::from_num_days_from_ce_opt(CE_EPOCH + value.date().days() as i32);
    match date {
        Some(date) => {
            let time = naive_time_from_time(value.time());
            NaiveDateTime::new(date, time)
                .format("%Y-%m-%d %H:%M:%S%.f")
                .to_string()
        }
        None => "NULL".to_string(),
    }
}

fn format_date_time_offset(value: &DateTimeOffset) -> String {
    let base = format_date_time2(&value.datetime2());
    let minutes = value.offset();
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.unsigned_abs();
    format!("{base}{sign}{:02}:{:02}", abs / 60, abs % 60)
}

/// `datetime`/`smalldatetime`：天数自 1900 起，秒以 1/300 秒为粒度。
fn format_legacy_datetime(days: i32, seconds_fragments: u32) -> String {
    let Some(date) = NaiveDate::from_num_days_from_ce_opt(DAYS_1900_TO_CE + days) else {
        return days.to_string();
    };
    let secs = seconds_fragments / 300;
    let subsec_nanos = (seconds_fragments % 300) * 10_000_000 / 300;
    match NaiveTime::from_num_seconds_from_midnight_opt(secs, subsec_nanos) {
        Some(time) => NaiveDateTime::new(date, time)
            .format("%Y-%m-%d %H:%M:%S%.f")
            .to_string(),
        None => days.to_string(),
    }
}

fn naive_time_from_time(time: Time) -> NaiveTime {
    let scale = time.scale().min(7) as u32;
    let total_nanos = time.increments() * 10u64.pow(9 - scale);
    let secs = (total_nanos / 1_000_000_000) as u32;
    let subsec_nanos = (total_nanos % 1_000_000_000) as u32;
    NaiveTime::from_num_seconds_from_midnight_opt(secs, subsec_nanos)
        .unwrap_or(NaiveTime::MIN)
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
    fn formats_time_with_scale() {
        // 1.5 秒，scale=1（1/10 秒粒度）；%.f 按 3 位一组裁剪尾零
        let time = Time::new(15, 1);
        assert_eq!(format_time(&time), "00:00:01.500");
        // 正午整，scale=7
        let noon = Time::new(432_000_000_000, 7);
        assert_eq!(format_time(&noon), "12:00:00");
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
        // 14:30:00.1234567
        let time = Time::new(522_001_234_567, 7);
        let dt = DateTime2::new(date, time);
        // chrono 的 %.f 按 3 位一组裁剪尾零，123456700ns 保留 9 位
        assert_eq!(format_date_time2(&dt), "2024-06-15 14:30:00.123456700");
    }

    #[test]
    fn formats_legacy_datetime() {
        let days = (NaiveDate::from_ymd_opt(2024, 6, 15).unwrap().num_days_from_ce()
            - DAYS_1900_TO_CE) as i32;
        // 14:30:00 = 300 fragments/秒 × 52200 秒
        assert_eq!(format_legacy_datetime(days, 300 * 52_200), "2024-06-15 14:30:00");
    }

    #[test]
    fn formats_datetime_offset() {
        let date = Date::new(
            NaiveDate::from_ymd_opt(2024, 1, 1)
                .unwrap()
                .num_days_from_ce() as u32
                - 1,
        );
        let time = Time::new(0, 0);
        let dt = DateTime2::new(date, time);
        let dto = DateTimeOffset::new(dt, 480); // UTC+8:00
        assert_eq!(format_date_time_offset(&dto), "2024-01-01 00:00:00+08:00");
        let dto_neg = DateTimeOffset::new(dt, -300); // UTC-5:00
        assert_eq!(
            format_date_time_offset(&dto_neg),
            "2024-01-01 00:00:00-05:00"
        );
    }

    #[test]
    fn formats_numeric_and_guid() {
        let numeric = Numeric::new_with_scale(12345, 2);
        assert_eq!(column_data_to_string(&ColumnData::Numeric(Some(numeric))), "123.45");
    }
}
