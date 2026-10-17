//! Manual smoke test of command mode's Responses call (PARLA-63):
//! `OPENAI_API_KEY=... cargo test --test command_live -- --ignored --nocapture`
use parla_lib::{
    command_mode::responses::{send, Request},
    settings::CommandSettings,
};

#[test]
#[ignore = "live: calls the OpenAI Responses API"]
fn gpt_6_luna_replies_with_text_and_usage() {
    let Some(key) = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
    else {
        println!("Skipping the command mode live test: OPENAI_API_KEY is unset or empty");
        return;
    };
    let defaults = CommandSettings::default();
    let started = std::time::Instant::now();
    let reply = send(
        &key,
        &Request {
            model: defaults.model,
            reasoning_effort: defaults.reasoning_effort,
            instruction: "Rispondi solo con la parola: pronto".into(),
        },
    )
    .expect("live reply");
    println!(
        "{:?} in {} ms ({:?} tokens in, {:?} out)",
        reply.text,
        started.elapsed().as_millis(),
        reply.tokens_in,
        reply.tokens_out
    );
    assert!(!reply.text.trim().is_empty());
    assert!(reply.tokens_in.is_some() && reply.tokens_out.is_some());
}
