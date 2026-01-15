use maud::{html, Markup, DOCTYPE};

pub struct PageConfig<'a> {
    pub title: &'a str,
    pub api_base: &'a str,
}

pub fn base(config: &PageConfig, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (config.title) }
                link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/bulma@0.9.4/css/bulma.min.css";
            }
            body {
                script {
                    (format!("const API_BASE = \"{}\";", config.api_base))
                }

                (content)

                script type="module" src="/static/loader.js" {}
            }
        }
    }
}
