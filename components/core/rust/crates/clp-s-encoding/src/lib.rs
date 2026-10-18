//! Byte-exact CLP variable encoding as written by the C++ `clp-s`: the placeholder bytes of
//! logtypes, the eight-byte encodings of integer and float variables, message reconstruction from
//! a logtype and its variables, and the tokenizer that finds potential variables in text (used to
//! encode search strings for the dictionary prefilter).

mod error;
mod message;
mod placeholder;
mod tokenizer;
mod variable;

pub use error::EncodingError;
pub use message::LogtypePlaceholders;
pub use message::decode_message;
pub use message::encode_message;
pub use placeholder::VariablePlaceholder;
pub use tokenizer::VariableToken;
pub use tokenizer::VariableTokenKind;
pub use tokenizer::VariableTokenizer;
pub use variable::EncodedVariable;
pub use variable::decode_dictionary_variable;
pub use variable::decode_float_variable;
pub use variable::decode_integer_variable;
pub use variable::encode_dictionary_variable;
pub use variable::encode_float_variable;
pub use variable::encode_integer_variable;
