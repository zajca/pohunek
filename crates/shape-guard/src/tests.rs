use serde::{Deserialize, Serialize};

use crate::assert_never_round_trips;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Arg {
    Literal(String),
    Reference,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Activity {
    Idle,
}

#[test]
#[should_panic(expected = "does not match the pattern it is listed under")]
fn a_sample_must_match_the_pattern_it_is_listed_under() {
    data_values!(
        Arg, None;
        Arg::Literal(..) => Arg::Reference,
        Arg::Reference => Arg::Reference,
    );
}

#[test]
#[should_panic(expected = "must not round-trip through strict serde")]
fn a_value_that_round_trips_is_not_accepted_as_unknown() {
    assert_never_round_trips(&Activity::Idle);
}
