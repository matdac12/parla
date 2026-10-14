use super::{openai::OpenAi, xai::Xai, SttProvider};
use std::sync::Arc;

pub fn all() -> Vec<Arc<dyn SttProvider>> {
    vec![
        Arc::new(OpenAi),
        Arc::new(Xai),
        Arc::new(super::parakeet::Parakeet),
    ]
}

pub fn get(id: &str) -> Option<Arc<dyn SttProvider>> {
    all().into_iter().find(|provider| provider.id() == id)
}
