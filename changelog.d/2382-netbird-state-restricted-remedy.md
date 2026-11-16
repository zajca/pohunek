---
kind: changed
breaking: true
---
`overlay::OverlayError::StateRestricted` has a required `remedy: String` field that carries the operator action. External provider implementations that construct or destructure the variant must handle the new field. ([#2382](https://github.com/zajca/pohunek/issues/2382))
