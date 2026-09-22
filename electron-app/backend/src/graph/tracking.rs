//! Physical write detection only. No value in this module proves a journal exists.
use super::{GraphError, GraphResult, PaperRecoveryArtifact, transaction::Transaction};
use crate::runtime::Cancellation;
use cozo::DataValue;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphWriteKind {
    Initialize,
    Apply,
    Restore,
    Reset,
    Forward,
    Compensate,
    CoordinatedReset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphJournalAuthority {
    Absent,
    ExternalUntrusted,
    /// Physical claim only: callers must still validate the durable journal.
    JournalTracked,
}

/// An immutable observation, not an authorization capability or a replay receipt.
/// Generation is local to this database, starts at zero and never rewinds on reset.
/// No public constructor, deserializer or metadata setter is provided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPublication {
    generation: i64,
    write_id: String,
    kind: GraphWriteKind,
    authority: GraphJournalAuthority,
    digest: Option<String>,
}

impl GraphPublication {
    pub fn generation(&self) -> i64 {
        self.generation
    }
    pub fn write_id(&self) -> &str {
        &self.write_id
    }
    pub fn kind(&self) -> GraphWriteKind {
        self.kind
    }
    pub fn journal_authority(&self) -> GraphJournalAuthority {
        self.authority
    }
    pub fn authority_digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }
}

/// Paper artifact and physical fence read under the same lock and transaction.
/// The artifact remains v1; this wrapper does not add journal authority to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPaperObservation {
    pub(super) publication: GraphPublication,
    pub(super) artifact: PaperRecoveryArtifact,
}
impl GraphPaperObservation {
    pub fn publication(&self) -> &GraphPublication {
        &self.publication
    }
    pub fn artifact(&self) -> &PaperRecoveryArtifact {
        &self.artifact
    }
}

pub(super) fn read(tx: &Transaction, c: &Cancellation) -> GraphResult<GraphPublication> {
    // Limit+1 detects a second row instead of ignoring a forged extra singleton.
    let rows = tx.run(
        "?[key, version, generation, write_id, transition, authority] := *graph_publication{key, version, generation, write_id, transition, authority} :limit 2",
        BTreeMap::new(), c,
    )?;
    let invalid = GraphError::InvalidSchema;
    let [row] = rows.rows.as_slice() else {
        return Err(invalid);
    };
    let [key, version, generation, write_id, transition, authority] = row.as_slice() else {
        return Err(invalid);
    };
    if key.get_str() != Some("store") || version.get_int() != Some(1) {
        return Err(invalid);
    }
    let generation = generation.get_int().filter(|g| *g >= 0).ok_or(invalid)?;
    let write_id = write_id.get_str().ok_or(invalid)?;
    let uuid = uuid::Uuid::parse_str(write_id).map_err(|_| invalid)?;
    if uuid.get_version() != Some(uuid::Version::Random)
        || uuid.get_variant() != uuid::Variant::RFC4122
        || uuid.to_string() != write_id
    {
        return Err(invalid);
    }
    let digest = authority.get_str().and_then(|s| s.strip_prefix("journal:"));
    let coordinated = digest.is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    let (kind, authority) = match (generation, transition.get_str(), authority.get_str()) {
        (0, Some("initialize"), Some("absent")) => {
            (GraphWriteKind::Initialize, GraphJournalAuthority::Absent)
        }
        (1.., Some("apply"), Some("external")) => (
            GraphWriteKind::Apply,
            GraphJournalAuthority::ExternalUntrusted,
        ),
        (1.., Some("restore"), Some("external")) => (
            GraphWriteKind::Restore,
            GraphJournalAuthority::ExternalUntrusted,
        ),
        (1.., Some("reset"), Some("external")) => (
            GraphWriteKind::Reset,
            GraphJournalAuthority::ExternalUntrusted,
        ),
        (1.., Some("forward"), _) if coordinated => (
            GraphWriteKind::Forward,
            GraphJournalAuthority::JournalTracked,
        ),
        (1.., Some("compensate"), _) if coordinated => (
            GraphWriteKind::Compensate,
            GraphJournalAuthority::JournalTracked,
        ),
        (1.., Some("coordinated_reset"), _) if coordinated => (
            GraphWriteKind::CoordinatedReset,
            GraphJournalAuthority::JournalTracked,
        ),
        _ => return Err(invalid),
    };
    Ok(GraphPublication {
        generation,
        write_id: write_id.into(),
        kind,
        authority,
        digest: digest.map(str::to_owned),
    })
}

pub(super) fn initialize(tx: &Transaction, c: &Cancellation) -> GraphResult<()> {
    write(tx, 0, "initialize", "absent", c)
}

/// Called once per transaction, including logical no-ops. Failure before COMMIT
/// rolls this back together with both edge directions. No caller-supplied identity.
pub(super) fn advance(tx: &Transaction, kind: GraphWriteKind, c: &Cancellation) -> GraphResult<()> {
    let next = read(tx, c)?
        .generation
        .checked_add(1)
        .ok_or(GraphError::GenerationOverflow)?;
    let transition = match kind {
        GraphWriteKind::Apply => "apply",
        GraphWriteKind::Restore => "restore",
        GraphWriteKind::Reset => "reset",
        _ => return Err(GraphError::InvalidInput),
    };
    write(tx, next, transition, "external", c)
}

fn write(
    tx: &Transaction,
    generation: i64,
    transition: &str,
    authority: &str,
    c: &Cancellation,
) -> GraphResult<()> {
    tx.run(
        "?[key, version, generation, write_id, transition, authority] <- [['store', 1, $generation, $write_id, $transition, $authority]] :put graph_publication {key => version, generation, write_id, transition, authority}",
        BTreeMap::from([
            ("generation".into(), DataValue::from(generation)),
            ("write_id".into(), DataValue::from(uuid::Uuid::new_v4().to_string())),
            ("transition".into(), DataValue::from(transition)),
            ("authority".into(), DataValue::from(authority)),
        ]), c,
    )?;
    Ok(())
}

pub(super) fn coordinated(
    tx: &Transaction,
    expected: &super::Fence,
    desired: &super::Fence,
    c: &Cancellation,
) -> GraphResult<()> {
    if &super::Fence::from_publication(&read(tx, c)?) != expected
        || desired.generation
            != expected
                .generation
                .checked_add(1)
                .ok_or(GraphError::GenerationOverflow)?
    {
        return Err(GraphError::InvalidArtifact);
    }
    desired.validate()?;
    tx.run(
        "?[key, version, generation, write_id, transition, authority] <- [['store', 1, $generation, $write_id, $transition, $authority]] :put graph_publication {key => version, generation, write_id, transition, authority}",
        BTreeMap::from([
            ("generation".into(), DataValue::from(desired.generation)),
            ("write_id".into(), DataValue::from(desired.id.clone())),
            ("transition".into(), DataValue::from(desired.direction.clone())),
            ("authority".into(), DataValue::from(format!("journal:{}", desired.digest))),
        ]), c,
    )?;
    Ok(())
}
