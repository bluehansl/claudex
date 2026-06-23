//! Renders the Claude team binding identity directly above the composer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::live_wrap::take_prefix_by_width;
use crate::render::renderable::Renderable;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TeammateIdentity {
    team: Option<String>,
    agent: Option<String>,
}

impl TeammateIdentity {
    pub(crate) fn new(team: Option<String>, agent: Option<String>) -> Self {
        Self {
            team: normalize_part(team),
            agent: normalize_part(agent),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.team.is_none() && self.agent.is_none()
    }

    fn text(&self) -> Option<String> {
        match (self.team.as_deref(), self.agent.as_deref()) {
            (Some(team), Some(agent)) => Some(format!("  {agent} - team {team}")),
            (Some(team), None) => Some(format!("  team {team}")),
            (None, Some(agent)) => Some(format!("  {agent}")),
            (None, None) => None,
        }
    }

    fn render_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width < 4 {
            return Vec::new();
        }
        let Some(text) = self.text() else {
            return Vec::new();
        };
        let (truncated, _, _) = take_prefix_by_width(&text, width as usize);
        vec![Line::from(truncated.dim())]
    }
}

impl Renderable for TeammateIdentity {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        Paragraph::new(self.render_lines(area.width)).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.render_lines(width).len() as u16
    }
}

fn normalize_part(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::buffer::Buffer;

    #[test]
    fn hidden_without_identity() {
        let identity = TeammateIdentity::default();
        assert!(identity.is_empty());
        assert_eq!(identity.desired_height(/*width*/ 80), 0);
    }

    #[test]
    fn renders_agent_and_team() {
        let identity =
            TeammateIdentity::new(Some("team-a".to_string()), Some("backend".to_string()));
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 1));

        identity.render(Rect::new(0, 0, 80, 1), &mut buf);

        assert_eq!(identity.desired_height(/*width*/ 80), 1);
        assert!(format!("{buf:?}").contains("backend - team team-a"));
    }
}
