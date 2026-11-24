//! Code of conduct templates offered by `ListCodesOfConduct`. The texts are original to
//! Beehive.

use bee_proto::v1::ModerationCodeOfConduct;

/// The templates, in key order.
pub fn templates() -> Vec<ModerationCodeOfConduct> {
    vec![
        ModerationCodeOfConduct {
            key: "agents_and_people".into(),
            name: "Agents and People".into(),
            body: "# Working together\n\n\
This project is built by people and by software agents. The same expectations apply to both.\n\n\
- Say who you are. Agents name their owner and their purpose in their profile.\n\
- Keep changes small and say why they exist. One idea per change request.\n\
- Do not flood. If you are an agent, respect the project's limits on open change requests and comments.\n\
- Treat reviewers' time as scarce. Reply to feedback or withdraw the change.\n\
- Criticize work, not people. Harassment, threats and discrimination end your participation.\n\n\
Maintainers may hide, lock, limit or block to keep the project healthy, and will say why when they can.\n"
                .into(),
            url: String::new(),
        },
        ModerationCodeOfConduct {
            key: "plain_courtesy".into(),
            name: "Plain Courtesy".into(),
            body: "# Plain courtesy\n\n\
Be kind and be clear.\n\n\
1. Assume good intent until shown otherwise.\n\
2. Disagree with the idea, never with the person.\n\
3. No harassment, slurs or personal attacks.\n\
4. Keep private information private.\n\
5. If something bothers you, report it to the maintainers; they will look at it.\n\n\
Repeated or serious breaches lead to hidden comments, locked threads, temporary limits or a block.\n"
                .into(),
            url: String::new(),
        },
        ModerationCodeOfConduct {
            key: "short_and_firm".into(),
            name: "Short and Firm".into(),
            body: "# Conduct\n\n\
Stay on topic. Be respectful. Do not spam, harass or impersonate.\n\n\
Maintainers decide what breaks these rules. Their actions are logged, and you may ask for a second look.\n"
                .into(),
            url: String::new(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_distinct_and_non_empty() {
        let t = templates();
        assert!(t.len() >= 2);
        let mut keys: Vec<_> = t.iter().map(|c| c.key.clone()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), t.len());
        assert!(
            t.iter()
                .all(|c| c.body.starts_with('#') && !c.name.is_empty())
        );
    }
}
