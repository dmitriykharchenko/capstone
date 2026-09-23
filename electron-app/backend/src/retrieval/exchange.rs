use super::{types::ExchangeVector, *};
use crate::history::CommittedTurn;
use serde::{Deserialize, Serialize};

/// Validated, bounded provider-free replay input. The enclosing shared journal
/// supplies the hash envelope; these bits retain exact finite vector values.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreparedExchangeArtifact {
    version: u32,
    identity: EmbeddingIdentity,
    exchange_id: i64,
    session_id: String,
    canonical_digest: String,
    context: String,
    vector_bits: Vec<String>,
}
impl PreparedExchangeArtifact {
    pub fn new(prepared: PreparedExchange, canonical: &CommittedTurn) -> RetrievalResult<Self> {
        let r = prepared.record;
        let result = Self {
            version: 1,
            identity: prepared.identity,
            exchange_id: r.exchange_id,
            session_id: r.session_id,
            canonical_digest: canonical.intent.canonical_digest.clone(),
            context: r.context,
            vector_bits: r
                .vector
                .iter()
                .map(|v| format!("{:016x}", v.to_bits()))
                .collect(),
        };
        result.verify(canonical)?;
        Ok(result)
    }
    pub fn verify(&self, canonical: &CommittedTurn) -> RetrievalResult<()> {
        self.restore(&self.identity)?;
        let e = &canonical.exchange;
        if self.exchange_id != e.exchange_id
            || self.session_id != e.session_id
            || self.canonical_digest != canonical.intent.canonical_digest
            || self.context
                != format!(
                    "User: {}\nAssistant: {}",
                    e.user_message.content, e.assistant_message.content
                )
        {
            return Err(RetrievalError::InvalidArtifact);
        }
        Ok(())
    }
    pub fn exchange_id(&self) -> i64 {
        self.exchange_id
    }
    pub fn restore(&self, identity: &EmbeddingIdentity) -> RetrievalResult<PreparedExchange> {
        if self.version != 1
            || &self.identity != identity
            || self.session_id.len() > 128
            || self.context.len() > 1_200_000
            || self.vector_bits.len() > 65_536
            || self.canonical_digest.len() != 64
        {
            return Err(RetrievalError::InvalidArtifact);
        }
        self.identity.validate()?;
        let vector = self
            .vector_bits
            .iter()
            .map(|s| {
                if s.len() != 16
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(RetrievalError::InvalidArtifact);
                }
                Ok(f64::from_bits(
                    u64::from_str_radix(s, 16).map_err(|_| RetrievalError::InvalidArtifact)?,
                ))
            })
            .collect::<RetrievalResult<Vec<_>>>()?;
        let record = ExchangeVector {
            exchange_id: self.exchange_id,
            session_id: self.session_id.clone(),
            context: self.context.clone(),
            vector,
        };
        record.validate(identity)?;
        Ok(PreparedExchange {
            identity: identity.clone(),
            record,
        })
    }
}
