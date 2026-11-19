---
kind: fixed
---
A stop of a frozen worker whose supervisor retirement changed nothing now rolls its durable stop intent back and schedules a supervision retry, so the session keeps its previous desired state instead of staying marked as stopping. ([#2007](https://github.com/zajca/pohunek/issues/2007))
