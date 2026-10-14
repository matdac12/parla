use parla_lib::{
    clip::Clip,
    provider::{xai::Xai, SttProvider},
    settings::Settings,
};

#[test]
fn transcribes_hello_when_key_is_available() {
    if std::env::var("XAI_API_KEY").map_or(true, |key| key.trim().is_empty()) {
        println!("Skipping xAI live test: XAI_API_KEY is unset or empty");
        return;
    }
    let clip = Clip::from_wav_bytes(include_bytes!("fixtures/hello.wav")).unwrap();
    let text = Xai
        .transcribe(&clip, &Settings::default())
        .expect("live transcription");
    assert!(!text.trim().is_empty());
}
