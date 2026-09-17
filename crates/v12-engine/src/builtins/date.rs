//! Date built-ins.
//!
//! A Date object is a `Kind::Date` whose single internal slot
//! `[[DateValue]]` lives in `properties[0]` as a Number (the time value in
//! milliseconds since the Unix epoch, or NaN for an invalid date). The slot
//! carries no shape descriptor, so it is invisible to property lookup and
//! enumeration; the JS surface is the shape-bound method set installed on
//! `Date.prototype` by the realm (see `crates/v12-engine/src/realm.rs`).
//!
//! ## Local time == UTC (v1)
//!
//! v1 has no IANA time-zone database, so every `LocalTime`/`UTC` conversion
//! is the identity and `getTimezoneOffset()` returns `+0`. The engine is
//! therefore only conformant for the UTC case; a host with a non-UTC TZ
//! would disagree with a real engine on the `getX`/`setX` (non-UTC)
//! methods. This is the documented v1 boundary, not a bug: the UTC methods
//! (`getUTCFullYear`, …) and `getTimezoneOffset() === 0` are exact.
//!
//! ## Algorithms
//!
//! The arithmetic follows ES2026 21.4 (Date Objects) directly: `Day`,
//! `TimeWithinDay`, `DayFromYear`, `YearFromTime`, `MakeDay`, `MakeTime`,
//! `TimeClip`, and the month/day normalization that lets out-of-range
//! components overflow (e.g. `new Date(2020, 12, 1)` is Jan 2021). All of it
//! is arithmetic in `f64` so the full ±8.64e15 ms range round-trips exactly.
//!
//! ## Coercion boundary
//!
//! A `NativeHandler` runs with a bare `&mut Heap` and cannot re-enter the
//! interpreter, so `ToNumber`/`ToPrimitive` of an *object* argument cannot
//! call user `valueOf`/`toString`/`@@toPrimitive` here. Date therefore uses
//! [`Ctx::to_number`] (objects → NaN) and [`Ctx::to_string`] for the
//! argument-conversion steps. The consequence is that
//! implementation-directed coercion tests (`coercion-order.js`,
//! `coercion-errors.js`, the `arg-*-to-number.js` family) fail on this lane;
//! the spec-arithmetic behavior is unaffected.

use v12_heap::{FunctionTarget, JsObject, JsValue, Kind};
use v12_native::{NativeId, Throw};

use super::ctx::Ctx;

/// Milliseconds per day.
const MS_PER_DAY: f64 = 86_400_000.0;
/// Milliseconds per hour.
const MS_PER_HOUR: f64 = 3_600_000.0;
/// Milliseconds per minute.
const MS_PER_MINUTE: f64 = 60_000.0;
/// Milliseconds per second.
const MS_PER_SECOND: f64 = 1000.0;
/// Maximum absolute time value (±100,000,000 days); beyond is NaN.
const MAX_TIME_VALUE: f64 = 8.64e15;

/// The `[[DateValue]]` slot index in a Date object's `properties`.
const SLOT_DATE_VALUE: usize = 0;

/// ES `TimeClip(time)`: NaN for non-finite or out-of-range; otherwise
/// `ToIntegerOrInfinity(time) + 0` (which also maps `-0` to `+0`).
#[must_use]
pub fn time_clip(time: f64) -> f64 {
    if !time.is_finite() || time.abs() > MAX_TIME_VALUE {
        return f64::NAN;
    }
    // `trunc` is ES ToIntegerOrInfinity for finite values; `+ 0.0` folds -0.
    time.trunc() + 0.0
}

/// ES `Day(t)`: the day number of the time value (floor division, so
/// negative times land on the mathematically correct day).
fn day(t: f64) -> f64 {
    (t / MS_PER_DAY).floor()
}

/// ES `TimeWithinDay(t)`: the time value modulo one day, in `[0, MS_PER_DAY)`.
fn time_within_day(t: f64) -> f64 {
    let r = t % MS_PER_DAY;
    if r < 0.0 { r + MS_PER_DAY } else { r }
}

/// Days from the epoch to 1 January of `y` (ES `DayFromYear`), valid for the
/// whole representable range.
fn day_from_year(y: f64) -> f64 {
    365.0 * (y - 1970.0) + ((y - 1969.0) / 4.0).floor() - ((y - 1901.0) / 100.0).floor()
        + ((y - 1601.0) / 400.0).floor()
}

/// True when `y` is a leap year (proleptic Gregorian).
fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Floor division for `i64` operands (Rust `/` truncates toward zero).
fn div_floor(a: i64, b: i64) -> i64 {
    let q = a / b;
    if a % b != 0 && (a < 0) != (b < 0) {
        q - 1
    } else {
        q
    }
}

/// Days from the epoch to 1 January of integer year `y`.
///
/// The spec's `DayFromYear(y) = 365(y − 1970) + ⌊(y−1969)/4⌋ −
/// ⌊(y−1901)/100⌋ + ⌊(y−1601)/400⌋`; with `d = y − 1970` the divisors are
/// `d+1`, `d+69`, and `d+369`.
fn days_from_epoch_to_year(y: i64) -> i64 {
    let d = y - 1970;
    let leap_days = div_floor(d + 1, 4) - div_floor(d + 69, 100) + div_floor(d + 369, 400);
    365 * d + leap_days
}

/// ES `InLeapYear(t)`.
fn in_leap_year(t: f64) -> f64 {
    if is_leap(year_from_time(t) as i64) {
        1.0
    } else {
        0.0
    }
}

/// ES `YearFromTime(t)`: the calendar year containing day number `day(t)`.
///
/// Estimates from the mean year length, then corrects by at most one step;
/// exact across the whole ±8.64e15 ms range.
fn year_from_time(t: f64) -> f64 {
    let d = day(t);
    let mut y = (d / 365.2425).floor() as i64 + 1970;
    loop {
        if d < days_from_epoch_to_year(y) as f64 {
            y -= 1;
            continue;
        }
        if d >= days_from_epoch_to_year(y + 1) as f64 {
            y += 1;
            continue;
        }
        return y as f64;
    }
}

/// ES `DayWithinYear(t)`.
fn day_within_year(t: f64) -> f64 {
    day(t) - day_from_year(year_from_time(t))
}

/// Cumulative days before month `m` (0-based) in a non-leap year.
const MONTH_STARTS: [f64; 13] = [
    0.0, 31.0, 59.0, 90.0, 120.0, 151.0, 181.0, 212.0, 243.0, 273.0, 304.0, 334.0, 365.0,
];

/// The day number of the first of 0-based `month` in year `year`.
///
/// The leap day shifts every month from March on, so `+1` applies only for
/// `month >= 2` (a January first is unaffected by the year's leap status).
fn month_start_day(year: f64, month: usize) -> f64 {
    day_from_year(year)
        + MONTH_STARTS[month]
        + if month >= 2 && is_leap(year as i64) {
            1.0
        } else {
            0.0
        }
}

/// ES `MonthFromTime(t)`: the 0-based month containing `t`.
///
/// The leap day sits at the END of February, so it shifts the start of month
/// index 2 (March) onward — hence the `>= 2` test on both bounds.
fn month_from_time(t: f64) -> f64 {
    let d = day_within_year(t);
    let leap = in_leap_year(t);
    for m in 0..12usize {
        let start = MONTH_STARTS[m] + if m >= 2 { leap } else { 0.0 };
        let end = MONTH_STARTS[m + 1] + if m + 1 >= 2 { leap } else { 0.0 };
        if d >= start && d < end {
            return m as f64;
        }
    }
    11.0
}

/// ES `DateFromTime(t)`: the 1-based day of the month containing `t`.
fn date_from_time(t: f64) -> f64 {
    let d = day_within_year(t);
    let leap = in_leap_year(t);
    let m = month_from_time(t) as usize;
    d - (MONTH_STARTS[m] + if m >= 2 { leap } else { 0.0 }) + 1.0
}

/// ES `WeekDay(t)`: 0 (Sunday) through 6 (Saturday). The epoch was a Thursday
/// (4), so `(day + 4) mod 7`.
fn week_day(t: f64) -> f64 {
    let r = (day(t) + 4.0) % 7.0;
    if r < 0.0 { r + 7.0 } else { r }
}

/// ES `HourFromTime(t)`.
fn hour_from_time(t: f64) -> f64 {
    (time_within_day(t) / MS_PER_HOUR).floor()
}
/// ES `MinFromTime(t)`.
fn min_from_time(t: f64) -> f64 {
    (time_within_day(t) / MS_PER_MINUTE).floor() % 60.0
}
/// ES `SecFromTime(t)`.
fn sec_from_time(t: f64) -> f64 {
    (time_within_day(t) / MS_PER_SECOND).floor() % 60.0
}
/// ES `msFromTime(t)`.
fn ms_from_time(t: f64) -> f64 {
    time_within_day(t) % MS_PER_SECOND
}

/// ES `MakeTime(hour, min, sec, ms)`: NaN when any input is non-finite,
/// otherwise the combined milliseconds (may exceed one day's range).
///
/// Each component is `ToIntegerOrInfinity`ed first (ES step 2), so
/// `Date.UTC(2016, 0, 1, 0.9)` truncates rather than keeping the fraction.
fn make_time(hour: f64, min: f64, sec: f64, ms: f64) -> f64 {
    if !hour.is_finite() || !min.is_finite() || !sec.is_finite() || !ms.is_finite() {
        return f64::NAN;
    }
    hour.trunc() * MS_PER_HOUR
        + min.trunc() * MS_PER_MINUTE
        + sec.trunc() * MS_PER_SECOND
        + ms.trunc()
}

/// ES `MakeDay(year, month, date)`: NaN when any input is non-finite;
/// otherwise the day number, with month overflow normalized.
///
/// Computed as `f64` in spec order so the rounding matches the reference
/// algorithms (`Date.UTC` precision tests depend on this).
fn make_day(year: f64, month: f64, date: f64) -> f64 {
    if !year.is_finite() || !month.is_finite() || !date.is_finite() {
        return f64::NAN;
    }
    let y = year.trunc();
    let m = month.trunc();
    let ym = y + (m / 12.0).floor();
    let mn = m % 12.0;
    let mn = if mn < 0.0 { mn + 12.0 } else { mn };
    month_start_day(ym, mn as usize) + date.trunc() - 1.0
}

/// ES `MakeDate(day, time)`.
fn make_date(day: f64, time: f64) -> f64 {
    day * MS_PER_DAY + time
}

/// ES `LocalTime(t)` / `UTC(t)`: the identity in v1 (local == UTC).
fn local_time(t: f64) -> f64 {
    t
}

/// Reads `[[DateValue]]` off a Date receiver, or `None` when `this` is not a
/// Date object (ES `thisTimeValue` step 2 → TypeError at the call site).
fn this_time_value(this: JsValue, ctx: &Ctx) -> Option<f64> {
    let obj = this.as_object()?;
    if ctx.heap.get(obj).kind != Kind::Date {
        return None;
    }
    let v = ctx.heap.get(obj).properties[SLOT_DATE_VALUE];
    Some(
        v.as_smi()
            .map(f64::from)
            .or_else(|| v.as_f64())
            .unwrap_or(f64::NAN),
    )
}

/// JS-visible number value: Smi when integral and in range, else a double.
fn js_number(n: f64) -> JsValue {
    super::helpers::js_number(n)
}

/// Writes `[[DateValue]]` on a Date object.
fn set_time_value(ctx: &mut Ctx, obj: v12_heap::Handle<JsObject>, t: f64) {
    let v = js_number(t);
    let slot = ctx
        .heap
        .get_mut(obj)
        .properties
        .get_mut(SLOT_DATE_VALUE)
        .expect("Date object always carries its [[DateValue]] slot");
    *slot = v;
}

/// Allocates a fresh Date object with `[[DateValue]] = t`, linking its
/// `[[Prototype]]` to the constructor's `prototype` field when supplied.
fn alloc_date(
    ctx: &mut Ctx,
    t: f64,
    proto: Option<v12_heap::Handle<JsObject>>,
) -> v12_heap::Handle<JsObject> {
    let mut obj = JsObject {
        kind: Kind::Date,
        ..JsObject::default()
    };
    obj.properties.push(js_number(t));
    obj.property_keys.push(None);
    let h = ctx.alloc_obj(obj);
    if let Some(proto) = proto {
        ctx.heap.get_mut(h).prototype = Some(proto);
    }
    h
}

/// True when `this` is the realm's `Date` constructor (i.e. the call arrived
/// through `new Date(...)`; the native seam passes the callee as `this`).
fn is_date_construct(ctx: &Ctx, this: JsValue) -> bool {
    this.as_object().is_some_and(|o| {
        ctx.heap.get(o).kind == Kind::Function
            && matches!(
                ctx.heap.get(o).callable,
                FunctionTarget::Bytecode(idx) if idx == u32::from(NativeId::DateConstruct)
            )
    })
}

/// `Date(...)` / `new Date(...)`.
///
/// Called as a function it returns the current time as a *string* (ES
/// 21.4.2.1); called with `new` it builds a Date object.
pub fn date_construct(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let proto = this.as_object().and_then(|o| ctx.heap.get(o).prototype);
    if !is_date_construct(ctx, this) {
        // `Date()`: the current time rendered by `toString`.
        let text = format_date_time(now_ms());
        return Ok(JsValue::string(ctx.heap.intern_text(&text)));
    }

    let t = match args.len() {
        0 => now_ms(),
        1 => {
            let v = args[0];
            if let Some(obj) = v.as_object()
                && ctx.heap.get(obj).kind == Kind::Date
            {
                this_time_value(v, ctx).unwrap_or(f64::NAN)
            } else if let Some(h) = v.as_string() {
                let text = ctx.string_text(h);
                parse_date(&text)
            } else {
                // ES 21.4.2.2 step 3: `tv = TimeClip(ToNumber(value))`, so a
                // fractional or infinite argument truncates to NaN/整数.
                time_clip(ctx.to_number(v))
            }
        }
        _ => {
            let mut nums = [0.0f64; 7];
            nums[2] = 1.0;
            for (i, slot) in nums.iter_mut().enumerate() {
                if let Some(&arg) = args.get(i) {
                    *slot = ctx.to_number(arg);
                }
            }
            let year = normalize_year(nums[0]);
            let final_date = make_date(
                make_day(year, nums[1], nums[2]),
                make_time(nums[3], nums[4], nums[5], nums[6]),
            );
            time_clip(local_time(final_date))
        }
    };

    let h = alloc_date(ctx, t, proto);
    Ok(JsValue::object(h))
}

/// ES 21.4.2.1 year rule: `0 ≤ ToInteger(y) ≤ 99` becomes `1900 + y`.
fn normalize_year(year: f64) -> f64 {
    if year.is_nan() {
        return year;
    }
    let yi = year.trunc();
    if (0.0..=99.0).contains(&yi) {
        1900.0 + yi
    } else {
        year
    }
}

/// Milliseconds since the epoch for "now".
fn now_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as f64,
        Err(e) => -(e.duration().as_millis() as f64),
    }
}

/// `Date.now()`.
pub fn date_now(_ctx: &mut Ctx, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    Ok(js_number(now_ms()))
}

/// Shared MakeDay/MakeTime/TimeClip body for `Date.UTC` and the multi-arg
/// constructor (year already normalized by the caller).
///
/// `year` is required, so a missing argument coerces `undefined` to NaN;
/// `month` defaults to `+0` and `date` to `1` (ES steps 2–7).
fn make_date_from_args(args: &[JsValue], ctx: &mut Ctx) -> f64 {
    let mut nums = [0.0f64; 7];
    nums[0] = f64::NAN;
    nums[2] = 1.0;
    for (i, slot) in nums.iter_mut().enumerate() {
        if let Some(&arg) = args.get(i) {
            *slot = ctx.to_number(arg);
        }
    }
    let year = normalize_year(nums[0]);
    let final_date = make_date(
        make_day(year, nums[1], nums[2]),
        make_time(nums[3], nums[4], nums[5], nums[6]),
    );
    time_clip(final_date)
}

/// `Date.UTC(year, month, date, hours, minutes, seconds, ms)`.
pub fn date_utc(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    Ok(js_number(make_date_from_args(args, ctx)))
}

/// `Date.parse(string)`.
pub fn date_parse(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = match args.first() {
        Some(&v) => ctx.to_string(v),
        None => "undefined".to_string(),
    };
    Ok(js_number(parse_date(&text)))
}

/// `Date.prototype.valueOf` / `getTime` — the receiver's time value.
pub fn date_proto_value_of(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let t = this_time_value(this, ctx)
        .ok_or_else(|| ctx.type_error("TypeError: this is not a Date object"))?;
    Ok(js_number(t))
}

/// `Date.prototype.getTimezoneOffset()` — always `+0` in v1 (local == UTC).
pub fn date_proto_get_timezone_offset(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let t = this_time_value(this, ctx)
        .ok_or_else(|| ctx.type_error("TypeError: this is not a Date object"))?;
    if t.is_nan() {
        return Ok(js_number(f64::NAN));
    }
    Ok(js_number(0.0))
}

/// Shared getter body: read the receiver, NaN-propagate, then apply `f` to
/// its (local) time value.
fn get_component(
    ctx: &mut Ctx,
    this: JsValue,
    f: impl FnOnce(f64) -> f64,
) -> Result<JsValue, Throw> {
    let t = this_time_value(this, ctx)
        .ok_or_else(|| ctx.type_error("TypeError: this is not a Date object"))?;
    if t.is_nan() {
        return Ok(js_number(f64::NAN));
    }
    Ok(js_number(f(local_time(t))))
}

macro_rules! date_getter {
    ($name:ident, $f:expr) => {
        pub fn $name(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
            get_component(ctx, this, $f)
        }
    };
}

date_getter!(date_proto_get_full_year, |t| year_from_time(t));
date_getter!(date_proto_get_month, |t| month_from_time(t));
date_getter!(date_proto_get_date, |t| date_from_time(t));
date_getter!(date_proto_get_day, |t| week_day(t));
date_getter!(date_proto_get_hours, |t| hour_from_time(t));
date_getter!(date_proto_get_minutes, |t| min_from_time(t));
date_getter!(date_proto_get_seconds, |t| sec_from_time(t));
date_getter!(date_proto_get_milliseconds, |t| ms_from_time(t));
date_getter!(date_proto_get_year, |t| year_from_time(t) - 1900.0);
// UTC variants are identical in v1 (local == UTC).
date_getter!(date_proto_get_utc_full_year, |t| year_from_time(t));
date_getter!(date_proto_get_utc_month, |t| month_from_time(t));
date_getter!(date_proto_get_utc_date, |t| date_from_time(t));
date_getter!(date_proto_get_utc_day, |t| week_day(t));
date_getter!(date_proto_get_utc_hours, |t| hour_from_time(t));
date_getter!(date_proto_get_utc_minutes, |t| min_from_time(t));
date_getter!(date_proto_get_utc_seconds, |t| sec_from_time(t));
date_getter!(date_proto_get_utc_milliseconds, |t| ms_from_time(t));

/// `Date.prototype.setTime(time)` — replaces `[[DateValue]]`.
pub fn date_proto_set_time(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = this
        .as_object()
        .filter(|o| ctx.heap.get(*o).kind == Kind::Date)
        .ok_or_else(|| ctx.type_error("TypeError: this is not a Date object"))?;
    let t = match args.first() {
        Some(&v) => ctx.to_number(v),
        None => f64::NAN,
    };
    let clipped = time_clip(t);
    set_time_value(ctx, obj, clipped);
    Ok(js_number(clipped))
}

/// Receiver plus the local time value the setters rebuild from.
struct SetterCtx {
    /// The Date receiver.
    obj: v12_heap::Handle<JsObject>,
    /// `LocalTime([[DateValue]])`, or `+0` when the spec's step 2 coerces NaN.
    t: f64,
}

/// Prepares a setter. `nan_to_zero` selects the spec's explicit
/// "if t is NaN, let t be +0" step (present in `setFullYear`/`setYear`, and
/// absent from the component setters, which let NaN propagate through
/// `MakeDate`).
fn setter_prepare(ctx: &mut Ctx, this: JsValue, nan_to_zero: bool) -> Result<SetterCtx, Throw> {
    let obj = this
        .as_object()
        .filter(|o| ctx.heap.get(*o).kind == Kind::Date)
        .ok_or_else(|| ctx.type_error("TypeError: this is not a Date object"))?;
    let raw = this_time_value(this, ctx).unwrap_or(f64::NAN);
    let t = if raw.is_nan() && nan_to_zero {
        0.0
    } else {
        local_time(raw)
    };
    Ok(SetterCtx { obj, t })
}

/// Finishes a setter: converts the rebuilt local date back to UTC, clips,
/// stores, and returns the new time value.
fn setter_finish(ctx: &mut Ctx, s: &SetterCtx, local_date: f64) -> Result<JsValue, Throw> {
    let u = time_clip(local_time(local_date));
    set_time_value(ctx, s.obj, u);
    Ok(js_number(u))
}

/// `Date.prototype.setFullYear(year[, month[, date]])`.
pub fn date_proto_set_full_year(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let s = setter_prepare(ctx, this, true)?;
    if s.t.is_nan() {
        // Should not happen for nan_to_zero, but keep NaN propagation honest.
        set_time_value(ctx, s.obj, f64::NAN);
        return Ok(js_number(f64::NAN));
    }
    // Argument reads happen after the [[DateValue]] read (spec order).
    let year = args.first().map_or(f64::NAN, |&v| ctx.to_number(v));
    let month = args
        .get(1)
        .map_or_else(|| month_from_time(s.t), |&v| ctx.to_number(v));
    let date = args
        .get(2)
        .map_or_else(|| date_from_time(s.t), |&v| ctx.to_number(v));
    if year.is_nan() {
        set_time_value(ctx, s.obj, f64::NAN);
        return Ok(js_number(f64::NAN));
    }
    let new_date = make_date(make_day(year, month, date), time_within_day(s.t));
    setter_finish(ctx, &s, new_date)
}

/// The UTC twin of `setFullYear` (identical in v1).
pub fn date_proto_set_utc_full_year(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    date_proto_set_full_year(ctx, this, args)
}

/// Shared time-component setter body (hours/minutes/seconds/ms).
///
/// `first_field` selects which successive field the first argument replaces
/// (0=hours, 1=minutes, 2=seconds, 3=ms). An invalid stored time keeps NaN
/// here (no `+0` coercion), so `MakeTime` propagates it.
fn set_time_fields(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
    first_field: usize,
) -> Result<JsValue, Throw> {
    let s = setter_prepare(ctx, this, false)?;
    let mut fields = [
        hour_from_time(s.t),
        min_from_time(s.t),
        sec_from_time(s.t),
        ms_from_time(s.t),
    ];
    for (i, field) in fields.iter_mut().enumerate().skip(first_field) {
        if let Some(&arg) = args.get(i - first_field) {
            *field = ctx.to_number(arg);
        }
    }
    let new_date = make_date(
        day(s.t),
        make_time(fields[0], fields[1], fields[2], fields[3]),
    );
    setter_finish(ctx, &s, new_date)
}

/// `Date.prototype.setHours(h[, m[, s[, ms]]])`.
pub fn date_proto_set_hours(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    set_time_fields(ctx, this, args, 0)
}
/// `Date.prototype.setUTCHours`.
pub fn date_proto_set_utc_hours(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    set_time_fields(ctx, this, args, 0)
}
/// `Date.prototype.setMinutes(m[, s[, ms]])`.
pub fn date_proto_set_minutes(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    set_time_fields(ctx, this, args, 1)
}
/// `Date.prototype.setUTCMinutes`.
pub fn date_proto_set_utc_minutes(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    set_time_fields(ctx, this, args, 1)
}
/// `Date.prototype.setSeconds(s[, ms])`.
pub fn date_proto_set_seconds(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    set_time_fields(ctx, this, args, 2)
}
/// `Date.prototype.setUTCSeconds`.
pub fn date_proto_set_utc_seconds(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    set_time_fields(ctx, this, args, 2)
}
/// `Date.prototype.setMilliseconds(ms)`.
pub fn date_proto_set_milliseconds(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    set_time_fields(ctx, this, args, 3)
}
/// `Date.prototype.setUTCMilliseconds`.
pub fn date_proto_set_utc_milliseconds(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    set_time_fields(ctx, this, args, 3)
}

/// `Date.prototype.setMonth(month[, date])`.
pub fn date_proto_set_month(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let s = setter_prepare(ctx, this, false)?;
    let month = args.first().map_or(f64::NAN, |&v| ctx.to_number(v));
    let date = args
        .get(1)
        .map_or_else(|| date_from_time(s.t), |&v| ctx.to_number(v));
    let new_date = make_date(
        make_day(year_from_time(s.t), month, date),
        time_within_day(s.t),
    );
    setter_finish(ctx, &s, new_date)
}

/// `Date.prototype.setUTCMonth`.
pub fn date_proto_set_utc_month(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    date_proto_set_month(ctx, this, args)
}

/// `Date.prototype.setDate(date)`.
pub fn date_proto_set_date(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let s = setter_prepare(ctx, this, false)?;
    let date = args.first().map_or(f64::NAN, |&v| ctx.to_number(v));
    let new_date = make_date(
        make_day(year_from_time(s.t), month_from_time(s.t), date),
        time_within_day(s.t),
    );
    setter_finish(ctx, &s, new_date)
}

/// `Date.prototype.setUTCDate`.
pub fn date_proto_set_utc_date(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    date_proto_set_date(ctx, this, args)
}

/// Annex B.2.4.2 `Date.prototype.setYear(year)`.
pub fn date_proto_set_year(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let s = setter_prepare(ctx, this, true)?;
    let y = args.first().map_or(f64::NAN, |&v| ctx.to_number(v));
    if y.is_nan() {
        set_time_value(ctx, s.obj, f64::NAN);
        return Ok(js_number(f64::NAN));
    }
    let yi = y.trunc();
    let year = if (0.0..=99.0).contains(&yi) {
        1900.0 + yi
    } else {
        y
    };
    let new_date = make_date(
        make_day(year, month_from_time(s.t), date_from_time(s.t)),
        time_within_day(s.t),
    );
    setter_finish(ctx, &s, new_date)
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `Date.prototype.toISOString` body: `None` for a non-finite time value
/// (the method then throws RangeError).
#[must_use]
pub fn to_iso_string(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    Some(format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        format_year_extended(year_from_time(t)),
        month_from_time(t) as i64 + 1,
        date_from_time(t) as i64,
        hour_from_time(t) as i64,
        min_from_time(t) as i64,
        sec_from_time(t) as i64,
        ms_from_time(t) as i64,
    ))
}

/// Year rendering for the Date Time String Format: `YYYY` in `[0, 9999]`,
/// extended `±YYYYYY` otherwise.
fn format_year_extended(year: f64) -> String {
    let y = year as i64;
    if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y > 9999 {
        format!("+{y:06}")
    } else {
        format!("-{:06}", -y)
    }
}

/// Year rendering for `toString`/`toUTCString`: at least four digits, with a
/// leading `-` for negative years.
fn format_year_plain(year: f64) -> String {
    let y = year as i64;
    if y < 0 {
        format!("-{:04}", -y)
    } else {
        format!("{y:04}")
    }
}

/// `toDateString` text, or `None` for an invalid date.
fn date_string(t: f64) -> Option<String> {
    if t.is_nan() {
        return None;
    }
    Some(format!(
        "{} {} {:02} {}",
        WEEKDAYS[week_day(t) as usize],
        MONTHS[month_from_time(t) as usize],
        date_from_time(t) as i64,
        format_year_plain(year_from_time(t)),
    ))
}

/// `toTimeString` body: `HH:mm:ss GMT+0000 (…UTC)` in v1.
fn time_string(t: f64) -> String {
    format!(
        "{:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
        hour_from_time(t) as i64,
        min_from_time(t) as i64,
        sec_from_time(t) as i64,
    )
}

/// `toString` text (ES `ToDateString`) for a time value.
fn format_date_time(t: f64) -> String {
    match date_string(t) {
        Some(ds) => format!("{ds} {}", time_string(t)),
        None => "Invalid Date".to_string(),
    }
}

/// `Date.prototype.toISOString`.
pub fn date_proto_to_iso_string(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let t = this_time_value(this, ctx)
        .ok_or_else(|| ctx.type_error("TypeError: this is not a Date object"))?;
    match to_iso_string(t) {
        Some(text) => Ok(JsValue::string(ctx.heap.intern_text(&text))),
        None => Err(ctx.range_error("RangeError: Invalid time value")),
    }
}

/// `Date.prototype.toString`.
pub fn date_proto_to_string(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let t = this_time_value(this, ctx)
        .ok_or_else(|| ctx.type_error("TypeError: this is not a Date object"))?;
    let text = format_date_time(t);
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
}

/// `Date.prototype.toDateString`.
pub fn date_proto_to_date_string(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let t = this_time_value(this, ctx)
        .ok_or_else(|| ctx.type_error("TypeError: this is not a Date object"))?;
    let text = date_string(t).unwrap_or_else(|| "Invalid Date".to_string());
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
}

/// `Date.prototype.toTimeString`.
pub fn date_proto_to_time_string(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let t = this_time_value(this, ctx)
        .ok_or_else(|| ctx.type_error("TypeError: this is not a Date object"))?;
    let text = if t.is_nan() {
        "Invalid Date".to_string()
    } else {
        time_string(t)
    };
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
}

/// `Date.prototype.toUTCString`.
pub fn date_proto_to_utc_string(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let t = this_time_value(this, ctx)
        .ok_or_else(|| ctx.type_error("TypeError: this is not a Date object"))?;
    let text = if t.is_nan() {
        "Invalid Date".to_string()
    } else {
        format!(
            "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
            WEEKDAYS[week_day(t) as usize],
            date_from_time(t) as i64,
            MONTHS[month_from_time(t) as usize],
            format_year_plain(year_from_time(t)),
            hour_from_time(t) as i64,
            min_from_time(t) as i64,
            sec_from_time(t) as i64,
        )
    };
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
}

/// `Date.prototype.toLocaleString` — best-effort `toString` (v1 has no
/// locale data; the test262 surface checks only the descriptor and arity).
pub fn date_proto_to_locale_string(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    date_proto_to_string(ctx, this, args)
}

/// `Date.prototype.toLocaleDateString` — best-effort `toDateString`.
pub fn date_proto_to_locale_date_string(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    date_proto_to_date_string(ctx, this, args)
}

/// `Date.prototype.toLocaleTimeString` — best-effort `toTimeString`.
pub fn date_proto_to_locale_time_string(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    date_proto_to_time_string(ctx, this, args)
}

/// `Date.prototype.toJSON(key)`.
///
/// ES 21.4.4.37: `ToObject(this)`, then the time value (NaN → `null`);
/// otherwise the ISO string. Generic non-Date receivers would `Invoke`
/// their own `toISOString`, which a native cannot do (no re-entry), so they
/// yield `null`.
pub fn date_proto_to_json(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = ctx.require_object_coercible(this)?;
    let t = obj
        .as_object()
        .filter(|o| ctx.heap.get(*o).kind == Kind::Date)
        .and_then(|_| this_time_value(obj, ctx))
        .unwrap_or(f64::NAN);
    if !t.is_finite() {
        return Ok(JsValue::null());
    }
    match to_iso_string(t) {
        Some(text) => Ok(JsValue::string(ctx.heap.intern_text(&text))),
        None => Ok(JsValue::null()),
    }
}

/// `Date.prototype[Symbol.toPrimitive](hint)`.
///
/// `"string"`/`"default"` try `toString` first, `"number"` tries `valueOf`
/// first, any other hint is a TypeError. The property lookup + call need
/// interpreter re-entry, which a `NativeHandler` cannot do; for a Date
/// receiver the two primitives are computed directly from `[[DateValue]]`,
/// and for a generic object the method is unreachable → TypeError. That
/// covers the Date surface while the generic branch stays spec-shaped.
pub fn date_proto_to_primitive(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    if !this.is_object() {
        return Err(
            ctx.type_error("TypeError: Date.prototype[Symbol.toPrimitive] called on non-object")
        );
    }
    let hint = args.first().copied().unwrap_or_else(JsValue::undefined);
    let hint_text = hint
        .as_string()
        .map(|h| ctx.string_text(h))
        .unwrap_or_default();
    let try_string_first = match hint_text.as_str() {
        "string" | "default" => true,
        "number" => false,
        _ => {
            return Err(ctx.type_error(
                "TypeError: Date.prototype[Symbol.toPrimitive] called with an invalid hint",
            ));
        }
    };
    // Date receiver fast path: the two primitives are `toString`/`valueOf`.
    if let Some(t) = this_time_value(this, ctx) {
        let text = if try_string_first {
            format_date_time(t)
        } else {
            // `valueOf` returns a Number; coercing to a string here would
            // change the type, so return the number itself.
            return Ok(js_number(t));
        };
        return Ok(JsValue::string(ctx.heap.intern_text(&text)));
    }
    Err(ctx.type_error("TypeError: Cannot convert object to primitive value"))
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parses a date string into a time value, or NaN.
///
/// The ES `Date Time String Format` (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`,
/// `YYYY-MM-DDTHH:mm`, `…:ss`, `…:ss.sss`, an optional `Z`/`±HH:mm` offset,
/// and extended `±YYYYYY` years) is handled exactly. A small set of
/// implementation-defined fallbacks (the RFC 2822-ish shapes `toString` and
/// `toUTCString` emit) is accepted so `Date.parse(x.toString())` round-trips.
#[must_use]
pub fn parse_date(text: &str) -> f64 {
    let s = text.trim();
    if s.is_empty() {
        return f64::NAN;
    }
    if let Some(t) = parse_iso(s) {
        return t;
    }
    parse_fallback(s)
}

/// Parses the ES Date Time String Format. Returns `None` for any deviation.
fn parse_iso(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    let mut i;
    // --- Year: four digits, or `±` and six digits --------------------
    let year: f64;
    if bytes.first().is_some_and(|b| *b == b'+' || *b == b'-') {
        if bytes.len() < 7 || !bytes[1..7].iter().all(u8::is_ascii_digit) {
            return None;
        }
        let sign = bytes[0] == b'-';
        let y: i64 = s[1..7].parse().ok()?;
        // "-000000" is invalid (negative zero as an extended year).
        if sign && y == 0 {
            return None;
        }
        year = if sign { -(y as f64) } else { y as f64 };
        i = 7;
    } else {
        if bytes.len() < 4 || !bytes[..4].iter().all(u8::is_ascii_digit) {
            return None;
        }
        year = s[..4].parse::<i64>().ok()? as f64;
        i = 4;
    }
    let mut month = 1.0f64;
    let mut day = 1.0f64;
    let mut hour = 0.0f64;
    let mut minute = 0.0f64;
    let mut second = 0.0f64;
    let mut ms = 0.0f64;
    let mut offset_minutes: Option<f64> = None;
    if bytes.get(i) == Some(&b'-') {
        i += 1;
        let m = parse_fixed(s, i, 2)?;
        if !(1..=12).contains(&m) {
            return None;
        }
        month = f64::from(m);
        i += 2;
        if bytes.get(i) == Some(&b'-') {
            i += 1;
            let d = parse_fixed(s, i, 2)?;
            if !(1..=31).contains(&d) {
                return None;
            }
            day = f64::from(d);
            i += 2;
        }
    }
    if i != bytes.len() {
        // A remaining part must be the time, introduced by `T`/`t`/space,
        // possibly followed directly by a zone designator.
        if bytes
            .get(i)
            .is_some_and(|b| *b == b'T' || *b == b't' || *b == b' ')
        {
            i += 1;
            hour = f64::from(parse_fixed(s, i, 2)?);
            if hour > 24.0 || bytes.get(i + 2) != Some(&b':') {
                return None;
            }
            i += 3;
            minute = f64::from(parse_fixed(s, i, 2)?);
            if minute > 59.0 {
                return None;
            }
            i += 2;
            if bytes.get(i) == Some(&b':') {
                i += 1;
                second = f64::from(parse_fixed(s, i, 2)?);
                if second > 59.0 {
                    return None;
                }
                i += 2;
                if bytes.get(i) == Some(&b'.') {
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    let frac = &s[start..i];
                    if frac.is_empty() {
                        return None;
                    }
                    let mut f = frac.to_string();
                    while f.len() < 3 {
                        f.push('0');
                    }
                    f.truncate(3);
                    ms = f.parse().ok()?;
                }
            }
        }
        match bytes.get(i).copied() {
            Some(b'Z') | Some(b'z') => {
                offset_minutes = Some(0.0);
                i += 1;
            }
            Some(b'+') | Some(b'-') => {
                let sign = if bytes[i] == b'-' { -1.0 } else { 1.0 };
                i += 1;
                let oh = f64::from(parse_fixed(s, i, 2)?);
                i += 2;
                if bytes.get(i) != Some(&b':') {
                    return None;
                }
                i += 1;
                let om = f64::from(parse_fixed(s, i, 2)?);
                i += 2;
                if oh > 23.0 || om > 59.0 {
                    return None;
                }
                offset_minutes = Some(sign * (oh * 60.0 + om));
            }
            _ => {}
        }
        if i != bytes.len() {
            return None;
        }
    }
    // `24:00` is only legal as exactly midnight.
    if hour == 24.0 && (minute != 0.0 || second != 0.0 || ms != 0.0) {
        return None;
    }
    let t = make_date(
        make_day(year, month - 1.0, day),
        make_time(hour, minute, second, ms),
    ) - offset_minutes.unwrap_or(0.0) * MS_PER_MINUTE;
    Some(time_clip(t))
}

/// Parses exactly `len` ASCII digits at byte offset `i`.
fn parse_fixed(s: &str, i: usize, len: usize) -> Option<u32> {
    let bytes = s.as_bytes();
    if i + len > bytes.len() || !bytes[i..i + len].iter().all(u8::is_ascii_digit) {
        return None;
    }
    s[i..i + len].parse::<u32>().ok()
}

/// Implementation-defined fallback: the shapes `toString` /
/// `toUTCString` / `toDateString` emit, in both orders —
///
/// - `Thu Jan 01 1970 00:00:00 GMT+0000 (UTC)` (`toString`/`toDateString`),
/// - `Thu, 01 Jan 1970 00:00:00 GMT` (`toUTCString`),
///
/// plus a bare `Jan 01 1970` date. An unrecognized string is NaN.
fn parse_fallback(s: &str) -> f64 {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let mut tokens = &tokens[..];
    // Optional leading weekday (`Thu` or `Thu,`).
    if let Some(tok) = tokens.first() {
        let bare = tok.trim_end_matches(',');
        if bare.len() >= 3 && WEEKDAYS.iter().any(|d| *d == &bare[..3]) {
            tokens = &tokens[1..];
        }
    }
    let Some(first) = tokens.first() else {
        return f64::NAN;
    };
    // Order detection: a leading numeric token is `DD Mon YYYY`; a leading
    // month name is `Mon DD YYYY`.
    let (day, month, rest) = if let Some(month) = month_index(first) {
        let Some(day_tok) = tokens.get(1) else {
            return f64::NAN;
        };
        let Ok(day) = day_tok.trim_end_matches(',').parse::<f64>() else {
            return f64::NAN;
        };
        (day, month, &tokens[2..])
    } else {
        let Ok(day) = first.trim_end_matches(',').parse::<f64>() else {
            return f64::NAN;
        };
        let Some(month) = tokens.get(1).and_then(|t| month_index(t)) else {
            return f64::NAN;
        };
        (day, month, &tokens[2..])
    };
    let Some(year_tok) = rest.first() else {
        return f64::NAN;
    };
    let Ok(year) = year_tok.trim_end_matches(',').parse::<i64>() else {
        return f64::NAN;
    };
    let rest = &rest[1..];
    let mut hour = 0.0f64;
    let mut minute = 0.0f64;
    let mut second = 0.0f64;
    let mut offset = 0.0f64;
    let mut rest = rest;
    if let Some(time_tok) = rest.first()
        && let Some((h, m, sec)) = parse_hms(time_tok)
    {
        hour = h;
        minute = m;
        second = sec;
        rest = &rest[1..];
    }
    for tok in rest {
        if tok.eq_ignore_ascii_case("GMT") || tok.eq_ignore_ascii_case("UTC") {
            continue;
        }
        if let Some(min) = parse_offset_token(tok) {
            offset = min;
        }
    }
    let t = make_date(
        make_day(year as f64, month, day),
        make_time(hour, minute, second, 0.0),
    ) - offset * MS_PER_MINUTE;
    time_clip(t)
}

/// 0-based month index from a three-letter English month name.
fn month_index(tok: &str) -> Option<f64> {
    MONTHS
        .iter()
        .position(|m| tok.len() >= 3 && m.eq_ignore_ascii_case(&tok[..3]))
        .map(|i| i as f64)
}

/// `HH:mm` or `HH:mm:ss`, returning `(h, m, s)`.
fn parse_hms(tok: &str) -> Option<(f64, f64, f64)> {
    let mut parts = tok.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = match parts.next() {
        Some(sec) => sec.parse().ok()?,
        None => 0.0,
    };
    Some((h, m, s))
}

/// Parses a `GMT±HHMM`/`±HH:MM` offset token into minutes. Zone
/// abbreviations such as `(PST)` are ignored (`None`).
fn parse_offset_token(tok: &str) -> Option<f64> {
    let t = tok
        .strip_prefix("GMT")
        .or_else(|| tok.strip_prefix("UTC"))
        .unwrap_or(tok);
    let (sign, rest) = if let Some(r) = t.strip_prefix('+') {
        (1.0, r)
    } else if let Some(r) = t.strip_prefix('-') {
        (-1.0, r)
    } else {
        return None;
    };
    let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 4 {
        return None;
    }
    let h: f64 = digits[..2].parse().ok()?;
    let m: f64 = digits[2..].parse().ok()?;
    Some(sign * (h * 60.0 + m))
}
