pub struct Question {
    text: String,
    choices: Vec<Choice>,
}

impl Question {
    pub fn new(text: &str, choices: &[Choice]) -> Self {
        Self {
            text: text.to_string(),
            choices: choices.to_vec(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone)]
pub struct Choice {
    id: String,
    text: String,
}

impl Choice {
    pub fn new(id: &str, text: &str) -> Self {
        Self {
            id: id.to_string(),
            text: text.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Answer {
    choice_id: String,
}

impl Answer {
    pub fn new(choice_id: String) -> Self {
        Self { choice_id }
    }

    pub fn choice_id(&self) -> &str {
        &self.choice_id
    }
}
