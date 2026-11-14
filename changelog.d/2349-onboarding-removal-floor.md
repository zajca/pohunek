---
kind: fixed
---
The onboarding guide's floor and its Claude deny rules for the `pohunek-onboard` launch profile now also refuse `pohunek session retention sweep --apply` and `pohunek plugin profile rm`, so an onboarding session never removes sessions or profiles, not even after a permission prompt. ([#2349](https://github.com/zajca/pohunek/issues/2349))
