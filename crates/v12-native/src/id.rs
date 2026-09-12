//! The [`NativeId`] enum: one collision-proof identifier per native function.
//!
//! Discriminants are the serialized callable index (an out-of-range bytecode
//! index the interpreter routes to the native seam). They are explicit and
//! stable: inserting a new variant never renumbers the others, and the
//! compiler rejects any accidental duplicate.

use strum::FromRepr;

/// One native function. Discriminants are explicit so inserting a variant
/// never renumbers the others (the serialized callable index is the
/// discriminant).
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, FromRepr)]
pub enum NativeId {
    // Object
    ObjectCreate = 1000,
    ObjectGetPrototypeOf = 1001,
    ObjectDefineProperty = 1002,
    ObjectEnumerableOwnKeys = 1003,
    ObjectKeys = 1004,
    ObjectValues = 1005,
    ObjectEntries = 1006,
    ObjectHasOwnProperty = 1007,
    ObjectProtoToString = 1008,
    ObjectProtoValueOf = 1009,
    ObjectAssign = 1010,
    ObjectIs = 1011,
    ObjectHasOwn = 1012,
    ObjectFreeze = 1013,
    ObjectIsFrozen = 1014,
    ObjectSeal = 1015,
    ObjectIsSealed = 1016,
    ObjectPreventExtensions = 1017,
    ObjectIsExtensible = 1018,
    ObjectFromEntries = 1019,
    ObjectGetOwnPropertyNames = 1020,
    ObjectGetOwnPropertyDescriptor = 1021,
    ObjectSetPrototypeOf = 1022,
    ObjectGetOwnPropertySymbols = 1023,
    /// `Object.prototype.propertyIsEnumerable(key)`.
    ObjectProtoPropertyIsEnumerable = 1024,
    // Array
    ArrayPush = 1100,
    ArrayPop = 1101,
    ArrayJoin = 1102,
    ArraySlice = 1107,
    ArraySort = 1108,
    ArrayForEach = 1109,
    ArrayMap = 1110,
    ArrayIsArray = 1106,
    ArrayIterator = 1103,
    ArrayIteratorEntries = 1104,
    ArrayIteratorKeys = 1105,
    ArrayIndexOf = 1111,
    ArrayLastIndexOf = 1112,
    ArrayIncludes = 1113,
    ArrayConcat = 1114,
    ArrayAt = 1115,
    ArrayReverse = 1116,
    ArrayShift = 1117,
    ArrayUnshift = 1118,
    ArraySplice = 1119,
    ArrayFill = 1120,
    ArrayFlat = 1121,
    ArrayToString = 1122,
    ArrayOf = 1123,
    ArrayFrom = 1124,
    ArrayFilter = 1125,
    ArraySome = 1126,
    ArrayEvery = 1127,
    ArrayFind = 1128,
    ArrayFindIndex = 1129,
    ArrayFindLast = 1130,
    ArrayFindLastIndex = 1131,
    ArrayReduce = 1132,
    ArrayReduceRight = 1133,
    ArrayFlatMap = 1134,
    ArrayCopyWithin = 1136,
    // String
    StringCharAt = 1200,
    StringSlice = 1201,
    StringConstruct = 1202,
    StringMatch = 1203,
    StringReplace = 1204,
    StringSearch = 1205,
    StringSplit = 1206,
    StringIndexOf = 1207,
    StringLastIndexOf = 1208,
    StringIncludes = 1209,
    StringStartsWith = 1210,
    StringEndsWith = 1211,
    StringCharCodeAt = 1212,
    StringCodePointAt = 1213,
    StringAt = 1214,
    StringConcat = 1215,
    StringRepeat = 1216,
    StringPadStart = 1217,
    StringPadEnd = 1218,
    StringTrim = 1219,
    StringTrimStart = 1220,
    StringTrimEnd = 1221,
    StringToLowerCase = 1222,
    StringToUpperCase = 1223,
    StringSubstring = 1224,
    StringSubstr = 1225,
    StringToString = 1226,
    StringValueOf = 1227,
    StringFromCharCode = 1228,
    StringFromCodePoint = 1229,
    StringReplaceAll = 1230,
    StringLocaleCompare = 1231,
    // Number / Math / Boolean / Error
    NumberIsNan = 1300,
    NumberIsFinite = 1301,
    NumberParseInt = 1302,
    NumberParseFloat = 1303,
    NumberConstruct = 1304,
    NumberIsInteger = 1305,
    NumberIsSafeInteger = 1306,
    NumberProtoToString = 1307,
    NumberToFixed = 1308,
    NumberToPrecision = 1309,
    NumberToExponential = 1310,
    NumberProtoValueOf = 1311,
    NumberConstMaxSafeInteger = 1312,
    NumberConstMinSafeInteger = 1313,
    NumberConstEpsilon = 1314,
    NumberConstMaxValue = 1315,
    NumberConstMinValue = 1316,
    NumberConstPositiveInfinity = 1317,
    NumberConstNegativeInfinity = 1318,
    NumberConstNaN = 1319,
    MathAbs = 1400,
    MathFloor = 1401,
    MathCeil = 1402,
    MathTrunc = 1403,
    MathPow = 1404,
    MathMax = 1405,
    MathMin = 1406,
    MathRandom = 1407,
    MathRound = 1408,
    MathSqrt = 1409,
    MathSign = 1410,
    MathCbrt = 1411,
    MathExp = 1412,
    MathExpm1 = 1413,
    MathLog = 1414,
    MathLog1p = 1415,
    MathLog2 = 1416,
    MathLog10 = 1417,
    MathSin = 1418,
    MathCos = 1419,
    MathTan = 1420,
    MathAsin = 1421,
    MathAcos = 1422,
    MathAtan = 1423,
    MathAtan2 = 1424,
    MathSinh = 1425,
    MathCosh = 1426,
    MathTanh = 1427,
    MathAsinh = 1428,
    MathAcosh = 1429,
    MathAtanh = 1430,
    MathHypot = 1431,
    MathClz32 = 1432,
    MathImul = 1433,
    MathFround = 1434,
    MathConstE = 1440,
    MathConstLn2 = 1441,
    MathConstLn10 = 1442,
    MathConstLog2e = 1443,
    MathConstLog10e = 1444,
    MathConstPi = 1445,
    MathConstSqrt1_2 = 1446,
    MathConstSqrt2 = 1447,
    BooleanConstruct = 1500,
    BooleanProtoToString = 1501,
    BooleanProtoValueOf = 1502,
    ErrorCreate = 1600,
    TypeErrorCreate = 1601,
    RangeErrorCreate = 1602,
    ReferenceErrorCreate = 1603,
    SyntaxErrorCreate = 1604,
    // JSON
    JsonParse = 2600,
    JsonStringify = 2601,
    // Global URI
    GlobalEncodeUri = 2404,
    GlobalDecodeUri = 2405,
    GlobalEncodeUriComponent = 2406,
    GlobalDecodeUriComponent = 2407,
    // Global
    GlobalIsNaN = 2400,
    GlobalIsFinite = 2401,
    GlobalParseInt = 2402,
    GlobalParseFloat = 2403,
    // Eval / function / console
    Eval = 1800,
    Function = 1801,
    FunctionCall = 1802,
    FunctionApply = 1803,
    FunctionBind = 1804,
    FunctionProtoToString = 1805,
    ConsoleLog = 1900,
    // Promise
    PromiseResolve = 1710,
    PromiseReject = 1711,
    PromiseThen = 1712,
    PromiseConstruct = 1713,
    PromiseCatch = 1714,
    QueueMicrotask = 1700,
    // Map / Set
    MapConstruct = 2000,
    MapGet = 2001,
    MapSet = 2002,
    MapHas = 2003,
    MapDelete = 2004,
    MapSize = 2005,
    MapIterator = 2006,
    MapClear = 2007,
    MapForEach = 2008,
    MapEntries = 2009,
    MapKeys = 2010,
    MapValues = 2011,
    SetConstruct = 2100,
    SetAdd = 2101,
    SetHas = 2102,
    SetDelete = 2103,
    SetSize = 2104,
    SetIterator = 2105,
    SetClear = 2106,
    SetForEach = 2107,
    SetEntries = 2108,
    SetKeys = 2109,
    SetValues = 2110,
    // Symbol
    SymbolConstruct = 2500,
    SymbolFor = 2501,
    SymbolKeyFor = 2502,
    SymbolProtoToString = 2503,
    SymbolProtoValueOf = 2504,
    SymbolProtoDescription = 2505,
    SymbolWellKnownIterator = 2506,
    SymbolWellKnownAsyncIterator = 2507,
    SymbolWellKnownHasInstance = 2508,
    SymbolWellKnownIsConcatSpreadable = 2509,
    SymbolWellKnownMatch = 2510,
    SymbolWellKnownReplace = 2511,
    SymbolWellKnownSearch = 2512,
    SymbolWellKnownSpecies = 2513,
    SymbolWellKnownSplit = 2514,
    SymbolWellKnownToPrimitive = 2515,
    SymbolWellKnownToStringTag = 2516,
    SymbolWellKnownUnscopables = 2517,
    // Iterator
    IteratorNext = 2200,
    IteratorSelf = 2204,
    IteratorMap = 2205,
    IteratorFilter = 2206,
    IteratorTake = 2207,
    IteratorDrop = 2208,
    IteratorFlatMap = 2209,
    IteratorReduce = 2210,
    IteratorToArray = 2211,
    IteratorForEach = 2212,
    IteratorSome = 2213,
    IteratorEvery = 2214,
    IteratorFind = 2215,
    IteratorFrom = 2216,
    // RegExp
    RegExpConstruct = 2300,
    RegExpExec = 2301,
    RegExpTest = 2302,
    RegExpToString = 2303,
    RegExpCompile = 2304,
    // Generator (interp-internal fallbacks, moved from the interp's NativeFn)
    GeneratorNext = 1910,
    GeneratorReturn = 1911,
    GeneratorThrow = 1912,
    /// Module-import hook: `import * as ns from "mod"` compiles to a call on
    /// this index, which the engine intercepts to build the namespace object
    /// (see `v12-bccompiler::model::NATIVE_IMPORT_INDEX`).
    ModuleImport = 254,
}

/// The serialized index of a native function.
impl From<NativeId> for u32 {
    #[inline]
    fn from(id: NativeId) -> u32 {
        id as u32
    }
}

/// A `u32` that does not name any [`NativeId`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnknownNativeId(pub u32);

impl std::fmt::Display for UnknownNativeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown native id {}", self.0)
    }
}

impl std::error::Error for UnknownNativeId {}

/// Decodes a serialized native index back into its enum variant.
///
/// The `match` is generated by [`strum::FromRepr`], so adding a variant never
/// requires updating it by hand.
impl TryFrom<u32> for NativeId {
    type Error = UnknownNativeId;

    #[inline]
    fn try_from(index: u32) -> Result<Self, Self::Error> {
        Self::from_repr(index).ok_or(UnknownNativeId(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_the_serialized_index() {
        for id in [
            NativeId::ObjectCreate,
            NativeId::ArrayPush,
            NativeId::StringSlice,
            NativeId::MathAbs,
            NativeId::MathFloor,
            NativeId::MathSqrt,
            NativeId::GlobalParseInt,
            NativeId::GlobalParseFloat,
            NativeId::NumberIsFinite,
            NativeId::NumberConstruct,
            NativeId::PromiseThen,
            NativeId::MapSet,
            NativeId::RegExpExec,
            NativeId::GeneratorNext,
        ] {
            let index: u32 = id.into();
            assert_eq!(NativeId::try_from(index), Ok(id), "id {id:?}");
        }
    }

    #[test]
    fn unknown_indexes_are_rejected() {
        assert!(NativeId::try_from(0).is_err());
        assert!(NativeId::try_from(999).is_err());
        assert!(NativeId::try_from(0xFFFF_FFFF).is_err());
        // A gap in the explicit discriminants is not a valid id.
        assert!(NativeId::try_from(1050).is_err());
    }
}
