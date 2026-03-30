// ===========================================================================
// Export — conversation serialization for training and archival.
//
// Converts Dyson's internal `Message` format into external formats used
// by the ML training ecosystem.
//
// Supported formats:
//   - ShareGPT: the de facto standard for fine-tuning chat models.
//     Used by Axolotl, LLaMA-Factory, OpenChat, and many others.
// ===========================================================================

pub mod sharegpt;
