// docs/jp-enhance/DESIGN.md §4.6 (ADR-F06): 音響エコーキャンセラー (AEC) の実体は
// LiveKit の `libwebrtc` (WebRTC APM) 実装だけだった。本フォークは通話機能を除去したので
// マイク入力とスピーカー出力が同時に走る経路が存在せず、AEC は不要。
// upstream が windows-gnu / FreeBSD 向けに持っていた no-op 実装を全ターゲットの唯一の
// 実装に格上げして `libwebrtc` 依存を切っている (これが `webrtc-sys` の prebuilt
// static lib = MSVC 14.43 STL シンボル要求の出所だった)。
//
// 出力ミキサーは今もこの型を通す (`audio_pipeline.rs::open_output_stream`) ので、
// 通話を復活させるならここを差し戻すだけでよい。

#[derive(Clone, Default)]
pub struct EchoCanceller;

impl EchoCanceller {
    pub fn process_reverse_stream(&mut self, _buf: &mut [i16]) {}
}
