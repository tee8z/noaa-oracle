use itertools::Itertools;

/// We are assuming the scoring mechanism does not allow for ties and every user has a unique score
/// One additional outcome is the "refund all" outcome.
pub fn generate_ranking_permutations(num_players: usize, rankings: usize) -> Vec<Vec<usize>> {
    let mut permutations = (0..num_players)
        .permutations(rankings)
        .collect::<Vec<Vec<usize>>>();

    // Always add the special "refund all" outcome
    permutations.push((0..num_players).collect());

    permutations
}

pub fn generate_outcome_messages(possible_user_outcomes: Vec<Vec<usize>>) -> Vec<Vec<u8>> {
    possible_user_outcomes
        .into_iter()
        .map(|inner_vec| {
            inner_vec
                .into_iter()
                .flat_map(|num| num.to_be_bytes())
                .collect::<Vec<u8>>()
        })
        .collect()
}

#[cfg(test)]
mod test {

    use super::generate_ranking_permutations;

    #[test]
    fn can_generate_list_of_winners_n5() {
        let num_players = 5;
        let permutations: Vec<Vec<usize>> = generate_ranking_permutations(num_players, 3);
        assert_eq!(permutations.len(), 61);
    }

    #[test]
    fn can_generate_list_of_winners_n20() {
        let num_players = 20;
        let permutations: Vec<Vec<usize>> = generate_ranking_permutations(num_players, 3);
        assert_eq!(permutations.len(), 6_841);
    }

    #[test]
    fn can_generate_list_of_winners_n25() {
        let num_players = 25;
        let permutations: Vec<Vec<usize>> = generate_ranking_permutations(num_players, 3);
        assert_eq!(permutations.len(), 13_801);
    }

    #[test]
    fn can_generate_list_of_winners_n100() {
        let num_players = 100;
        let permutations: Vec<Vec<usize>> = generate_ranking_permutations(num_players, 3);
        assert_eq!(permutations.len(), 970_201);
    }

    #[test]
    #[ignore = "Slow test, skip for ci"]
    fn can_generate_list_of_winners_n200() {
        let num_players = 200;
        let permutations: Vec<Vec<usize>> = generate_ranking_permutations(num_players, 3);
        assert_eq!(permutations.len(), 7_880_401);
    }

    #[test]
    #[ignore = "Slow test, skip for ci"]
    //note: beyond 500 players the time to create the permutations is over 60 seconds
    fn can_generate_list_of_winners_n400() {
        let num_players = 400;
        let permutations: Vec<Vec<usize>> = generate_ranking_permutations(num_players, 3);
        assert_eq!(permutations.len(), 63_520_801);
    }
}
