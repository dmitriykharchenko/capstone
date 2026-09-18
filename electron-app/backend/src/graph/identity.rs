use super::{GraphError, GraphResult, NodeKind};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

// Python str.split also considers these four ASCII separators whitespace.
pub(super) fn whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub(super) fn compact(value: &str) -> GraphResult<String> {
    let normalized: String = value.nfkc().collect();
    let text = normalized
        .split(whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() || text.contains('\0') {
        return Err(GraphError::InvalidInput);
    }
    Ok(text)
}

pub fn normalize_node_name(value: &str) -> GraphResult<String> {
    Ok(casefold(&compact(value)?))
}

pub fn stable_node_id(kind: NodeKind, value: &str) -> GraphResult<String> {
    let prefix = match kind {
        NodeKind::Author => "author",
        NodeKind::Topic => "topic",
        NodeKind::Paper => return Err(GraphError::InvalidInput),
    };
    let digest = Sha256::digest(normalize_node_name(value)?.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{prefix}:{hex}"))
}

// Unicode default full case folding differs from lowercase for these mappings
// and Cherokee (whose identity form is uppercase). Keep expansions: lowercase
// alone silently changes stable IDs for e.g. Straße, final sigma, and ligatures.
// This table encodes the full-fold exceptions, not locale-specific Turkic folds.
const FULL_FOLD: &str = "b5=3bc df=73,73 149=2bc,6e 17f=73 1f0=6a,30c 345=3b9
390=3b9,308,301 3b0=3c5,308,301 3c2=3c3 3d0=3b2 3d1=3b8 3d5=3c6 3d6=3c0
3f0=3ba 3f1=3c1 3f5=3b5 587=565,582 1c80=432 1c81=434 1c82=43e 1c83=441
1c84=442 1c85=442 1c86=44a 1c87=463 1c88=a64b 1e96=68,331 1e97=74,308
1e98=77,30a 1e99=79,30a 1e9a=61,2be 1e9b=1e61 1e9e=73,73 1f50=3c5,313
1f52=3c5,313,300 1f54=3c5,313,301 1f56=3c5,313,342
1f80=1f00,3b9 1f81=1f01,3b9 1f82=1f02,3b9 1f83=1f03,3b9
1f84=1f04,3b9 1f85=1f05,3b9 1f86=1f06,3b9 1f87=1f07,3b9
1f88=1f00,3b9 1f89=1f01,3b9 1f8a=1f02,3b9 1f8b=1f03,3b9
1f8c=1f04,3b9 1f8d=1f05,3b9 1f8e=1f06,3b9 1f8f=1f07,3b9
1f90=1f20,3b9 1f91=1f21,3b9 1f92=1f22,3b9 1f93=1f23,3b9
1f94=1f24,3b9 1f95=1f25,3b9 1f96=1f26,3b9 1f97=1f27,3b9
1f98=1f20,3b9 1f99=1f21,3b9 1f9a=1f22,3b9 1f9b=1f23,3b9
1f9c=1f24,3b9 1f9d=1f25,3b9 1f9e=1f26,3b9 1f9f=1f27,3b9
1fa0=1f60,3b9 1fa1=1f61,3b9 1fa2=1f62,3b9 1fa3=1f63,3b9
1fa4=1f64,3b9 1fa5=1f65,3b9 1fa6=1f66,3b9 1fa7=1f67,3b9
1fa8=1f60,3b9 1fa9=1f61,3b9 1faa=1f62,3b9 1fab=1f63,3b9
1fac=1f64,3b9 1fad=1f65,3b9 1fae=1f66,3b9 1faf=1f67,3b9
1fb2=1f70,3b9 1fb3=3b1,3b9 1fb4=3ac,3b9 1fb6=3b1,342 1fb7=3b1,342,3b9
1fbc=3b1,3b9 1fbe=3b9 1fc2=1f74,3b9 1fc3=3b7,3b9 1fc4=3ae,3b9
1fc6=3b7,342 1fc7=3b7,342,3b9 1fcc=3b7,3b9 1fd2=3b9,308,300
1fd3=3b9,308,301 1fd6=3b9,342 1fd7=3b9,308,342 1fe2=3c5,308,300
1fe3=3c5,308,301 1fe4=3c1,313 1fe6=3c5,342 1fe7=3c5,308,342
1ff2=1f7c,3b9 1ff3=3c9,3b9 1ff4=3ce,3b9 1ff6=3c9,342 1ff7=3c9,342,3b9
1ffc=3c9,3b9 fb00=66,66 fb01=66,69 fb02=66,6c fb03=66,66,69 fb04=66,66,6c
fb05=73,74 fb06=73,74 fb13=574,576 fb14=574,565 fb15=574,56b fb16=57e,576 fb17=574,56d";

pub(super) fn casefold(text: &str) -> String {
    use std::{collections::BTreeMap, sync::OnceLock};
    static TABLE: OnceLock<BTreeMap<char, String>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let character = |hex| char::from_u32(u32::from_str_radix(hex, 16).unwrap()).unwrap();
        FULL_FOLD
            .split_whitespace()
            .map(|entry| {
                let (key, value) = entry.split_once('=').unwrap();
                (character(key), value.split(',').map(character).collect())
            })
            .collect()
    });
    let mut result = String::with_capacity(text.len());
    for c in text.chars() {
        if let Some(folded) = table.get(&c) {
            result.push_str(folded);
        } else if ('\u{13a0}'..='\u{13f5}').contains(&c) {
            result.push(c);
        } else if ('\u{ab70}'..='\u{abbf}').contains(&c) || ('\u{13f8}'..='\u{13fd}').contains(&c) {
            result.extend(c.to_uppercase());
        } else {
            result.extend(c.to_lowercase());
        }
    }
    result
}
