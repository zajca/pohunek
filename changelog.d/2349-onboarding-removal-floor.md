---
kind: fixed
---
The onboarding guide's floor and the Claude deny rules of its `pohunek-onboard` launch profile now also cover `pohunek session retention sweep --apply` and `pohunek plugin profile rm`: written as the guide writes them, both are refused instead of only prompting. Like the guide's other `Bash` deny rules, they match the usual spellings of a command and are not a security boundary. ([#2349](https://github.com/zajca/pohunek/issues/2349))
