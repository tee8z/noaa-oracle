//! Compare decoded sustained wind with the METAR body, never with gusts or remarks.

pub(super) fn validate_wind(
    raw: &str,
    speed: Option<i64>,
    direction: Option<i64>,
) -> Result<(), String> {
    let groups: Vec<_> = raw
        .split_whitespace()
        .take_while(|token| *token != "RMK")
        .filter(|token| token.ends_with("KT") || token.ends_with("MPS"))
        .collect();
    if groups.is_empty() && speed.is_none() && direction.is_none() {
        return Ok(());
    }
    if groups.len() != 1 {
        return Err("no unambiguous raw sustained-wind group".into());
    }
    let group = groups[0];
    let (body, factor) = if let Some(body) = group.strip_suffix("KT") {
        (body, 1.0)
    } else {
        (group.strip_suffix("MPS").unwrap(), 1.9438444924406)
    };
    if !body.is_ascii() || body.len() < 5 {
        return Err("malformed raw sustained-wind group".into());
    }
    let (bearing, rest) = body.split_at(3);
    let mut parts = rest.split('G');
    let digits = parts.next().unwrap();
    let unsigned = |value: &str| {
        (matches!(value.len(), 2 | 3) && value.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| value.parse::<i64>().ok())
            .flatten()
    };
    let Some(raw_speed) = unsigned(digits) else {
        return Err("malformed raw sustained-wind speed".into());
    };
    if let Some(gust) = parts.next()
        && (unsigned(gust).is_none() || parts.next().is_some())
    {
        return Err("malformed raw gust group".into());
    }
    let expected_direction = if bearing == "VRB" {
        None
    } else if bearing.bytes().all(|byte| byte.is_ascii_digit()) {
        let bearing = bearing.parse::<i64>().unwrap();
        if !(0..=360).contains(&bearing) {
            return Err("raw wind direction outside 0..360 degrees".into());
        }
        Some(bearing)
    } else {
        return Err("malformed raw wind direction".into());
    };
    let expected_speed = (raw_speed as f64 * factor).round() as i64;
    if speed != Some(expected_speed) {
        return Err(format!(
            "decoded sustained wind {speed:?} disagrees with raw {expected_speed} knots"
        ));
    }
    // Both 0 and 360 mean north; VRB has no numerical direction.
    if direction.map(|value| value % 360) != expected_direction.map(|value| value % 360) {
        return Err("decoded wind direction disagrees with raw METAR".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_wind;

    #[test]
    fn sustained_wind_is_not_gust_or_peak_remark() {
        let raw = "METAR KORD 271851Z 23012G25KT 10SM 19/06 RMK PK WND 27035/1830";
        assert!(validate_wind(raw, Some(12), Some(230)).is_ok());
        assert!(validate_wind(raw, Some(25), Some(230)).is_err());
    }

    #[test]
    fn variable_calm_north_and_mps_have_explicit_semantics() {
        assert!(validate_wind("VRB03KT", Some(3), None).is_ok());
        assert!(validate_wind("VRB03KT", Some(3), Some(0)).is_err());
        assert!(validate_wind("00000KT", Some(0), Some(0)).is_ok());
        assert!(validate_wind("36004KT", Some(4), Some(0)).is_ok());
        assert!(validate_wind("18005MPS", Some(10), Some(180)).is_ok());
    }

    #[test]
    fn missing_ambiguous_and_malformed_groups_cannot_verify_decoded_wind() {
        assert!(validate_wind("060/03", Some(3), Some(60)).is_err());
        assert!(validate_wind("23012KT 24013KT", Some(12), Some(230)).is_err());
        assert!(validate_wind("23012GXXKT", Some(12), Some(230)).is_err());
        assert!(validate_wind("", None, None).is_ok());
        assert!(validate_wind("23012KT", None, None).is_err());
    }
}
