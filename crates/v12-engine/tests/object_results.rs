//! Object result arrays and Error.prototype.toString regressions.

use v12_engine::Engine;

fn eval_display(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

#[test]
fn object_result_arrays_use_array_prototype_and_constructor() {
    assert_eq!(
        eval_display(
            "var keys = Object.keys({a: 1}); \
             var values = Object.values({a: 1}); \
             var entries = Object.entries({a: 1}); \
             [keys.constructor === Array, \
              Object.getPrototypeOf(keys) === Array.prototype, \
              values.constructor === Array, \
              Object.getPrototypeOf(values) === Array.prototype, \
              entries.constructor === Array, \
              Object.getPrototypeOf(entries) === Array.prototype, \
              entries[0].constructor === Array, \
              Object.getPrototypeOf(entries[0]) === Array.prototype].join(',')"
        ),
        "true,true,true,true,true,true,true,true"
    );
}

#[test]
fn error_prototype_to_string_is_installed() {
    assert_eq!(
        eval_display("new TypeError('x').toString()"),
        "TypeError: x"
    );
}

#[test]
fn error_to_string_handles_empty_and_custom_fields() {
    assert_eq!(
        eval_display(
            "function result(error, name, message) { \
               if (name !== undefined) error.name = name; \
               if (message !== undefined) error.message = message; \
               return error.toString(); \
             } \
             [result(new Error()), \
              result(new Error(), '', 'message'), \
              result(new Error('message'), 'Custom', ''), \
              result(new Error(), 'Custom', 'detail')].join('|')"
        ),
        "Error|message|Custom|Custom: detail"
    );
}
