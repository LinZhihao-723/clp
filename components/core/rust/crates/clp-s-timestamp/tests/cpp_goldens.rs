//! Golden cases derived from the C++ `clp-s`: the marshalling assertions of its timestamp-parser
//! unit tests, and the timestamps that C++ `clp-s x` printed for every pattern stored in the
//! fixture and dataset archives.

use clp_s_timestamp::TimestampPattern;

/// The `(timestamp, pattern, epoch ns)` cases of "Timestamps are parsed accurately." in
/// `clp_s/timestamp_parser/test/test_TimestampParser.cpp`, where C++ asserts that marshalling the
/// epoch with the pattern (unquoted and quoted) prints the timestamp.
const CPP_MARSHAL_CASES: &[(&str, &str, i64)] = &[
    (
        "2015-02-01T01:02:03.004",
        r"\Y-\m-\dT\H:\M:\S.\3",
        1_422_752_523_004_000_000,
    ),
    (
        "2015-02-01T01:02:03.004005",
        r"\Y-\m-\dT\H:\M:\S.\6",
        1_422_752_523_004_005_000,
    ),
    (
        "2015-02-01T01:02:03.004005006",
        r"\Y-\m-\dT\H:\M:\S.\9",
        1_422_752_523_004_005_006,
    ),
    (
        "2015-02-01T01:02:03,004",
        r"\Y-\m-\dT\H:\M:\S,\3",
        1_422_752_523_004_000_000,
    ),
    (
        "[2015-02-01T01:02:03",
        r"[\Y-\m-\dT\H:\M:\S",
        1_422_752_523_000_000_000,
    ),
    (
        "[20150201-01:02:03]",
        r"[\Y\m\d-\H:\M:\S]",
        1_422_752_523_000_000_000,
    ),
    (
        "2015-02-01 01:02:03,004",
        r"\Y-\m-\d \H:\M:\S,\3",
        1_422_752_523_004_000_000,
    ),
    (
        "2015-02-01 01:02:03.004",
        r"\Y-\m-\d \H:\M:\S.\3",
        1_422_752_523_004_000_000,
    ),
    (
        "[2015-02-01 01:02:03,004]",
        r"[\Y-\m-\d \H:\M:\S,\3]",
        1_422_752_523_004_000_000,
    ),
    (
        "2015-02-01 01:02:03",
        r"\Y-\m-\d \H:\M:\S",
        1_422_752_523_000_000_000,
    ),
    (
        "2015/02/01 01:02:03",
        r"\Y/\m/\d \H:\M:\S",
        1_422_752_523_000_000_000,
    ),
    (
        "15/02/01 01:02:03",
        r"\y/\m/\d \H:\M:\S",
        1_422_752_523_000_000_000,
    ),
    (
        "150201  1:02:03",
        r"\y\m\d \k:\M:\S",
        1_422_752_523_000_000_000,
    ),
    (
        "01 Feb 2015 01:02:03,004",
        r"\d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \Y \H:\M:\S,\3",
        1_422_752_523_004_000_000,
    ),
    (
        "Feb 01, 2015  1:02:03 AM",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d, \Y \l:\M:\S \p",
        1_422_752_523_000_000_000,
    ),
    (
        "Feb 01, 2015 01:02:03 AM",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d, \Y \I:\M:\S \p",
        1_422_752_523_000_000_000,
    ),
    (
        "Feb 01, 2015 12:02:03 AM",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d, \Y \l:\M:\S \p",
        1_422_748_923_000_000_000,
    ),
    (
        "Feb 01, 2015 12:02:03 PM",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d, \Y \l:\M:\S \p",
        1_422_792_123_000_000_000,
    ),
    (
        "February 01, 2015 01:02",
        concat!(
            r"\B{January,February,March,April,May,June,July,August,September,October,",
            r"November,December} \d, \Y \H:\M",
        ),
        1_422_752_520_000_000_000,
    ),
    (
        "[01/Feb/2015:01:02:03",
        r"[\d/\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec}/\Y:\H:\M:\S",
        1_422_752_523_000_000_000,
    ),
    (
        "Sun Feb  1 01:02:03 2015",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat} \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,",
            r"Nov,Dec} \e \H:\M:\S \Y",
        ),
        1_422_752_523_000_000_000,
    ),
    (
        "<<<2015-02-01 01:02:03:004",
        r"<<<\Y-\m-\d \H:\M:\S:\3",
        1_422_752_523_004_000_000,
    ),
    (
        "Jan 21 11:56:42",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S",
        1_771_002_000_000_000,
    ),
    (
        "01-21 11:56:42.392",
        r"\m-\d \H:\M:\S.\3",
        1_771_002_392_000_000,
    ),
    (
        "2015/01/31 15:50:45.123",
        r"\Y/\m/\d \H:\M:\S.\3",
        1_422_719_445_123_000_000,
    ),
    (
        "2015/01/31 15:50:45,123",
        r"\Y/\m/\d \H:\M:\S,\3",
        1_422_719_445_123_000_000,
    ),
    (
        "2015/01/31T15:50:45",
        r"\Y/\m/\dT\H:\M:\S",
        1_422_719_445_000_000_000,
    ),
    (
        "2015/01/31T15:50:45.123",
        r"\Y/\m/\dT\H:\M:\S.\3",
        1_422_719_445_123_000_000,
    ),
    (
        "2015/01/31T15:50:45,123",
        r"\Y/\m/\dT\H:\M:\S,\3",
        1_422_719_445_123_000_000,
    ),
    (
        "2015-01-31T15:50:45",
        r"\Y-\m-\dT\H:\M:\S",
        1_422_719_445_000_000_000,
    ),
    ("1762445893", r"\E", 1_762_445_893_000_000_000),
    ("1762445893001", r"\L", 1_762_445_893_001_000_000),
    ("1762445893001002", r"\C", 1_762_445_893_001_002_000),
    ("1762445893001002003", r"\N", 1_762_445_893_001_002_003),
    ("1762445893.001", r"\E.\3", 1_762_445_893_001_000_000),
    ("1762445893.001002", r"\E.\6", 1_762_445_893_001_002_000),
    ("1762445893.001002003", r"\E.\9", 1_762_445_893_001_002_003),
    ("1762445893.001002000", r"\E.\9", 1_762_445_893_001_002_000),
    ("1762445893.00100201", r"\E.\T", 1_762_445_893_001_002_010),
    ("1762445893.1", r"\E.\T", 1_762_445_893_100_000_000),
    ("-1762445893", r"\E", -1_762_445_893_000_000_000),
    ("-1762445893001", r"\L", -1_762_445_893_001_000_000),
    ("-1762445893001002", r"\C", -1_762_445_893_001_002_000),
    ("-1762445893001002003", r"\N", -1_762_445_893_001_002_003),
    ("-1762445893.001", r"\E.\3", -1_762_445_893_001_000_000),
    ("-1762445893.001002", r"\E.\6", -1_762_445_893_001_002_000),
    (
        "-1762445893.001002003",
        r"\E.\9",
        -1_762_445_893_001_002_003,
    ),
    (
        "-1762445893.001002000",
        r"\E.\9",
        -1_762_445_893_001_002_000,
    ),
    ("-1762445893.00100201", r"\E.\T", -1_762_445_893_001_002_010),
    ("-1762445893.1", r"\E.\T", -1_762_445_893_100_000_000),
    (
        "Jan 21 11:56:42Z",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\SZ",
        1_771_002_000_000_000,
    ),
    (
        "Jan 21 11:56:42 UTC-01",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{-01}",
        1_774_602_000_000_000,
    ),
    (
        "Jan 21 11:56:42 UTC-01:30",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{-01:30}",
        1_776_402_000_000_000,
    ),
    (
        "Jan 21 11:56:42 UTC-0130",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{-0130}",
        1_776_402_000_000_000,
    ),
    (
        "Jan 21 11:56:42 EST",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S \o{EST,-0500}",
        1_789_002_000_000_000,
    ),
    (
        "Jan 21 11:56:42 UT",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UT",
        1_771_002_000_000_000,
    ),
    (
        "Jan 21 11:56:42 GMT",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S GMT",
        1_771_002_000_000_000,
    ),
    (
        "Thu, 21 Dec 2000 16:01:07 +0200",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S \z{+0200}",
        ),
        977_407_267_000_000_000,
    ),
    (
        "Thu, 21 Dec 2000 16:01:07 EST",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S \o{EST,-0500}",
        ),
        977_432_467_000_000_000,
    ),
    (
        "Jan 21 11:56:42 UTC+01",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{+01}",
        1_767_402_000_000_000,
    ),
    (
        "Jan 21 11:56:42 UTC+01:30",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{+01:30}",
        1_765_602_000_000_000,
    ),
    (
        "Jan 21 11:56:42 UTC+0130",
        r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d \H:\M:\S UTC\z{+0130}",
        1_765_602_000_000_000,
    ),
    (
        "Sat, 09 Mar 2024 15:04:05 -0500",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S \z{-0500}",
        ),
        1_710_014_645_000_000_000,
    ),
    (
        "Sat, 09 Mar 2024 15:04 -0500",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M \z{-0500}",
        ),
        1_710_014_640_000_000_000,
    ),
    (
        "Sat, 09 Mar 24 15:04:05 -0500",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \y \H:\M:\S \z{-0500}",
        ),
        1_710_014_645_000_000_000,
    ),
    (
        "Sat, 09 Mar 24 15:04 -0500",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \y \H:\M \z{-0500}",
        ),
        1_710_014_640_000_000_000,
    ),
    (
        "Sat,  9 Mar 2024 15:04:05 -0500",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \e \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S \z{-0500}",
        ),
        1_710_014_645_000_000_000,
    ),
    (
        "Sat,  9 Mar 2024 15:04 -0500",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \e \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M \z{-0500}",
        ),
        1_710_014_640_000_000_000,
    ),
    (
        "Sat,  9 Mar 24 15:04:05 -0500",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \e \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \y \H:\M:\S \z{-0500}",
        ),
        1_710_014_645_000_000_000,
    ),
    (
        "Sat,  9 Mar 24 15:04 -0500",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \e \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \y \H:\M \z{-0500}",
        ),
        1_710_014_640_000_000_000,
    ),
    (
        "09 Mar 2024 15:04:05 -0500",
        r"\d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \Y \H:\M:\S \z{-0500}",
        1_710_014_645_000_000_000,
    ),
    (
        "09 Mar 2024 15:04 -0500",
        r"\d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \Y \H:\M \z{-0500}",
        1_710_014_640_000_000_000,
    ),
    (
        "09 Mar 24 15:04:05 -0500",
        r"\d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \y \H:\M:\S \z{-0500}",
        1_710_014_645_000_000_000,
    ),
    (
        "09 Mar 24 15:04 -0500",
        r"\d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \y \H:\M \z{-0500}",
        1_710_014_640_000_000_000,
    ),
    (
        " 9 Mar 2024 15:04:05 -0500",
        r"\e \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \Y \H:\M:\S \z{-0500}",
        1_710_014_645_000_000_000,
    ),
    (
        " 9 Mar 2024 15:04 -0500",
        r"\e \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \Y \H:\M \z{-0500}",
        1_710_014_640_000_000_000,
    ),
    (
        " 9 Mar 24 15:04:05 -0500",
        r"\e \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \y \H:\M:\S \z{-0500}",
        1_710_014_645_000_000_000,
    ),
    (
        " 9 Mar 24 15:04 -0500",
        r"\e \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \y \H:\M \z{-0500}",
        1_710_014_640_000_000_000,
    ),
    (
        "01 Jan 69 00:00:00 UTC",
        r"\d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \y \H:\M:\S UTC",
        -31_536_000_000_000_000,
    ),
    (
        "31 Dec 68 23:59:59 UTC",
        r"\d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \y \H:\M:\S UTC",
        3_124_223_999_000_000_000,
    ),
    (
        "01 Jan 00 00:00:00 UTC",
        r"\d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \y \H:\M:\S UTC",
        946_684_800_000_000_000,
    ),
    (
        "Sun, 01 Jan 1950 00:00:00 UTC",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S UTC",
        ),
        -631_152_000_000_000_000,
    ),
    (
        "Wed, 31 Dec 1969 23:59:59 UTC",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S UTC",
        ),
        -1_000_000_000,
    ),
    (
        "Tue, 29 Feb 2000 12:00:00 UTC",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S UTC",
        ),
        951_825_600_000_000_000,
    ),
    (
        "Sun, 29 Feb 2004 23:59:59 PST",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S \o{PST,-0800}",
        ),
        1_078_127_999_000_000_000,
    ),
    (
        "Sun, 29 Feb 2004 23:59:60 PST",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\J \o{PST,-0800}",
        ),
        1_078_127_999_000_000_000,
    ),
    (
        "Sun, 10 Mar 2024 01:59:59 EST",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S \o{EST,-0500}",
        ),
        1_710_053_999_000_000_000,
    ),
    (
        "Sun, 10 Mar 2024 03:00:00 EDT",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S \o{EDT,-0400}",
        ),
        1_710_054_000_000_000_000,
    ),
    (
        "Sun, 03 Nov 2024 01:59:59 EDT",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S \o{EDT,-0400}",
        ),
        1_730_613_599_000_000_000,
    ),
    (
        "Sun, 03 Nov 2024 01:00:00 EST",
        concat!(
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,",
            r"Oct,Nov,Dec} \Y \H:\M:\S \o{EST,-0500}",
        ),
        1_730_613_600_000_000_000,
    ),
    (
        "1895-11-20T21:55:46,010",
        r"\Y-\m-\dT\H:\M:\S,\3",
        -2_338_769_053_990_000_000,
    ),
    (
        "2016-12-31T23:59:59,999Z",
        r"\Y-\m-\dT\H:\M:\S,\3Z",
        1_483_228_799_999_000_000,
    ),
    (
        "2016-12-31T23:59:60,999Z",
        r"\Y-\m-\dT\H:\M:\J,\3Z",
        1_483_228_799_999_000_000,
    ),
    (
        "2017-01-01T00:00:00,999Z",
        r"\Y-\m-\dT\H:\M:\S,\3Z",
        1_483_228_800_999_000_000,
    ),
];

/// `(pattern, epoch ns, C++ output)` for every pattern stored in the fixture and dataset archives,
/// with the text C++ `clp-s x` printed (`tests/fixtures/oracles`, `$PROTO_DATA_ROOT/oracles`).
const CPP_ORACLE_CASES: &[(&str, i64, &str)] = &[
    (
        r#""\Y-\m-\dT\H:\M:\SZ""#,
        1_700_000_000_000_000_000,
        r#""2023-11-14T22:13:20Z""#,
    ),
    (
        r#""\Y-\m-\dT\H:\M:\S.\3Z""#,
        1_700_000_001_123_000_000,
        r#""2023-11-14T22:13:21.123Z""#,
    ),
    (
        r#""\Y-\m-\dT\H:\M:\S.\3Z""#,
        1_700_000_014_500_000_000,
        r#""2023-11-14T22:13:34.500Z""#,
    ),
    (
        r#""\Y-\m-\dT\H:\M:\S.\6\z{+08:00}""#,
        1_699_971_202_123_456_000,
        r#""2023-11-14T22:13:22.123456+08:00""#,
    ),
    (
        r#""\Y-\m-\d \H:\M:\S,\3""#,
        1_700_000_003_123_000_000,
        r#""2023-11-14 22:13:23,123""#,
    ),
    (
        r#""\Y-\m-\dT\H:\M:\S""#,
        1_700_000_004_000_000_000,
        r#""2023-11-14T22:13:24""#,
    ),
    (
        r#""\Y/\m/\d \H:\M:\S.\T""#,
        1_700_000_005_500_000_000,
        r#""2023/11/14 22:13:25.5""#,
    ),
    (
        r#""[\Y-\m-\dT\H:\M:\S]""#,
        1_700_000_006_000_000_000,
        r#""[2023-11-14T22:13:26]""#,
    ),
    (
        r#""\d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \Y \H:\M:\S.\3""#,
        1_700_000_007_123_000_000,
        r#""14 Nov 2023 22:13:27.123""#,
    ),
    (
        r#""\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec} \d, \Y \l:\M:\S \p""#,
        1_700_000_008_000_000_000,
        r#""Nov 14, 2023 10:13:28 PM""#,
    ),
    (
        concat!(
            r#""\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat} \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,"#,
            r#"Oct,Nov,Dec} \e \H:\M:\S \Y""#,
        ),
        1_700_000_009_000_000_000,
        r#""Tue Nov 14 22:13:29 2023""#,
    ),
    (
        concat!(
            r#""\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,"#,
            r#"Sep,Oct,Nov,Dec} \Y \H:\M:\S GMT""#,
        ),
        1_700_000_010_000_000_000,
        r#""Tue, 14 Nov 2023 22:13:30 GMT""#,
    ),
    (
        concat!(
            r#""\A{Sun,Mon,Tue,Wed,Thu,Fri,Sat}, \d \B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,"#,
            r#"Sep,Oct,Nov,Dec} \Y \H:\M:\S \z{+0000}""#,
        ),
        1_700_000_011_000_000_000,
        r#""Tue, 14 Nov 2023 22:13:31 +0000""#,
    ),
    (
        r#""\y-\m-\d \H:\M:\S""#,
        1_700_000_012_000_000_000,
        r#""23-11-14 22:13:32""#,
    ),
    (r#""\E""#, 1_700_000_033_000_000_000, r#""1700000033""#),
    (
        r#""\Y-\m-\dT\H:\M:\S.\3\z{-05:00}""#,
        1_700_018_016_000_000_000,
        r#""2023-11-14T22:13:36.000-05:00""#,
    ),
    (
        r#""\Y-\m-\dT\H:\M:\S.\9Z""#,
        1_700_000_017_123_456_789,
        r#""2023-11-14T22:13:37.123456789Z""#,
    ),
    (
        r#""\Y-\m-\dT\H:\M:\S.\3Z""#,
        1_700_000_003_000_000_000,
        r#""2023-11-14T22:13:23.000Z""#,
    ),
    (r"\L", 1_700_000_036_000_000_000, "1700000036000"),
    (r"\L", 1_700_000_011_999_000_000, "1700000011999"),
    (r"\E", 1_700_000_002_000_000_000, "1700000002"),
    (r"\E", 0, "0"),
    (r"\E", -5_000_000_000_000, "-5000"),
    (r"\C", 1_700_000_003_123_456_000, "1700000003123456"),
    (r"\N", 1_700_000_004_123_456_789, "1700000004123456789"),
    (r"\E.\3", 1_700_000_000_123_000_000, "1700000000.123"),
    (r"\E.\3", 1_700_000_003_000_000_000, "1700000003.000"),
    (r"\E.\T", 1_700_000_001_500_000_000, "1700000001.5"),
    (r"\E.\T", 1_700_000_002_123_456_789, "1700000002.123456789"),
    (r"\E.\T", 1_700_000_006_999_999_000, "1700000006.999999"),
    (r"\E.\T", 2_899_553_342_221_251_328, "2899553342.221251328"),
    (r"\E", 1_700_000_009_000_000_000, "1700000009"),
    (
        r#""\Y-\m-\dT\H:\M:\S.\3Z""#,
        1_700_000_000_123_000_000,
        r#""2023-11-14T22:13:20.123Z""#,
    ),
    (
        r#""\Y-\m-\d \H:\M:\S,\3""#,
        1_700_000_003_001_000_000,
        r#""2023-11-14 22:13:23,001""#,
    ),
    (r"\L", 1_735_689_600_009_000_000, "1735689600009"),
    (
        r#""\Y-\m-\dT\H:\M:\S.\6Z""#,
        1_787_695_003_323_386_000,
        r#""2026-08-25T21:56:43.323386Z""#,
    ),
    (
        r#""\Y-\m-\dT\H:\M:\S.\3Z""#,
        1_767_225_609_896_000_000,
        r#""2026-01-01T00:00:09.896Z""#,
    ),
];

/// # Returns
///
/// `epoch_ns` formatted with `pattern` by [`TimestampPattern::format`].
///
/// # Panics
///
/// Panics if the pattern does not compile or cannot format `epoch_ns`.
fn formatted(pattern: &str, epoch_ns: i64) -> String {
    let mut output = Vec::new();
    TimestampPattern::parse(pattern)
        .expect("the pattern should compile")
        .format(epoch_ns, &mut output)
        .expect("the timestamp should be formattable");
    String::from_utf8(output).expect("formatted timestamps of these patterns are UTF-8")
}

/// # Returns
///
/// `epoch_ns` formatted with `pattern` by [`TimestampPattern::format_json`].
///
/// # Panics
///
/// Panics if the pattern does not compile or cannot format `epoch_ns`.
fn formatted_json(pattern: &str, epoch_ns: i64) -> String {
    let mut output = Vec::new();
    TimestampPattern::parse(pattern)
        .expect("the pattern should compile")
        .format_json(epoch_ns, &mut output)
        .expect("the timestamp should be formattable");
    String::from_utf8(output).expect("formatted timestamps of these patterns are UTF-8")
}

#[test]
fn cpp_marshal_cases_round_trip() {
    for &(text, pattern, epoch_ns) in CPP_MARSHAL_CASES {
        assert_eq!(formatted(pattern, epoch_ns), text, "pattern {pattern}");
        assert_eq!(formatted_json(pattern, epoch_ns), text, "pattern {pattern}");
        let quoted_pattern = format!("\"{pattern}\"");
        assert_eq!(
            formatted_json(&quoted_pattern, epoch_ns),
            format!("\"{text}\""),
            "pattern {quoted_pattern}"
        );
        assert_eq!(
            formatted(&quoted_pattern, epoch_ns),
            text,
            "pattern {quoted_pattern}"
        );
    }
}

// The JSON-escape case of the same C++ test: `\\` marshals as the JSON escape `\\`.
#[test]
fn backslashes_marshal_as_json_escapes() {
    let pattern = r"\Y\\\m\\\dT\H:\M:\S.\3";
    let epoch_ns = 1_422_752_523_004_000_000;
    assert_eq!(
        formatted_json(pattern, epoch_ns),
        r"2015\\02\\01T01:02:03.004"
    );
    assert_eq!(formatted(pattern, epoch_ns), r"2015\02\01T01:02:03.004");
}

#[test]
fn archive_patterns_format_like_the_cpp_oracles() {
    for &(pattern, epoch_ns, expected) in CPP_ORACLE_CASES {
        assert_eq!(
            formatted_json(pattern, epoch_ns),
            expected,
            "pattern {pattern}"
        );
    }
}
