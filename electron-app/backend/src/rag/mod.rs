use crate::{
    domain::{Message, RagResult},
    registry::BackendFuture,
    runtime::RequestContext,
};

pub trait RagPipeline: Send + Sync {
    // Stateless shared Chat/Evals path. Public progress only; initial 2–5 hops,
    // one coverage review, <=2 followups, <=7 hops and <=35 aggregate sources.
    fn run(
        &self,
        question: String,
        history: Vec<Message>,
        context: RequestContext,
    ) -> BackendFuture<RagResult>;
}
