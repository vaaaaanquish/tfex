//! Which other declarations an expression refers to.
//!
//! Expressions are walked, not evaluated: every traversal that has the shape
//! of a Terraform reference (`aws_vpc.main.id`, `var.region`,
//! `module.network.vpc_id`, ...) is a candidate. Whether a candidate really
//! names something is decided later, against what the directory declares;
//! that is what tells `aws_vpc.main` from `each.value` or a `for` variable.

use hcl_edit::Span;
use hcl_edit::expr::{Expression, Traversal, TraversalOperator};
use hcl_edit::structure::Body;
use hcl_edit::template::Template;
use hcl_edit::visit::{self, Visit};

/// A candidate reference: the address it would name, and the byte offset of
/// the expression in the text it was parsed from.
pub(super) type Candidate = (String, usize);

/// The candidate references in a block body, nested blocks included.
pub(super) fn in_body(body: &Body) -> Vec<Candidate> {
    let mut collector = Collector::default();
    collector.visit_body(body);
    collector.found
}

/// The candidate references in one expression.
pub(super) fn in_expression(expression: &Expression) -> Vec<Candidate> {
    let mut collector = Collector::default();
    collector.visit_expr(expression);
    collector.found
}

/// The candidate references in a string of Terraform's JSON syntax, which is
/// a template such as `"${aws_vpc.main.id}/24"`. Text that is not a valid
/// template refers to nothing.
pub(super) fn in_json_string(text: &str) -> Vec<Candidate> {
    let Ok(template) = text.parse::<Template>() else {
        return Vec::new();
    };
    let mut collector = Collector::default();
    collector.visit_template(&template);
    collector.found
}

/// The candidate reference that a string of Terraform's JSON syntax is when
/// it must be a bare reference, as the items of `depends_on` are.
pub(super) fn in_json_reference(text: &str) -> Vec<Candidate> {
    text.parse::<Expression>()
        .map_or_default(|expression| in_expression(&expression))
}

#[derive(Default)]
struct Collector {
    found: Vec<Candidate>,
}

impl Visit for Collector {
    fn visit_traversal(&mut self, node: &Traversal) {
        if let Some(address) = address_of(node) {
            let offset = node.span().map_or(0, |span| span.start);
            self.found.push((address, offset));
        }
        // An index may itself hold references: `aws_subnet.all[local.zone]`.
        visit::visit_traversal(self, node);
    }
}

/// The address a traversal would refer to: `var.<name>`, `local.<name>`,
/// `module.<name>`, `data.<type>.<name>`, or `<type>.<name>` for a managed
/// resource. What follows the address (attributes, indexes, splats) is
/// dropped.
fn address_of(traversal: &Traversal) -> Option<String> {
    let Expression::Variable(root) = &traversal.expr else {
        return None;
    };
    let mut attributes = traversal
        .operators
        .iter()
        .map_while(|operator| match operator.value() {
            TraversalOperator::GetAttr(name) => Some(name.as_str()),
            _ => None,
        });
    let first = attributes.next()?;
    match root.as_str() {
        "data" => Some(format!("data.{first}.{}", attributes.next()?)),
        scope => Some(format!("{scope}.{first}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addresses(candidates: Vec<Candidate>) -> Vec<String> {
        candidates.into_iter().map(|(address, _)| address).collect()
    }

    fn in_text(expression: &str) -> Vec<String> {
        addresses(in_expression(&expression.parse().unwrap()))
    }

    #[test]
    fn addresses_stop_where_the_attributes_begin() {
        assert_eq!(in_text("aws_vpc.main.id"), ["aws_vpc.main"]);
        assert_eq!(in_text("aws_subnet.all[0].id"), ["aws_subnet.all"]);
        assert_eq!(in_text("aws_subnet.all[*].id"), ["aws_subnet.all"]);
        assert_eq!(
            in_text("data.aws_region.current.name"),
            ["data.aws_region.current"]
        );
        assert_eq!(in_text("module.network.vpc_id"), ["module.network"]);
        assert_eq!(in_text("module.network"), ["module.network"]);
        assert_eq!(in_text("var.region"), ["var.region"]);
        assert_eq!(in_text("local.tags"), ["local.tags"]);
    }

    #[test]
    fn incomplete_references_are_not_candidates() {
        assert_eq!(in_text("aws_vpc"), Vec::<String>::new());
        assert_eq!(in_text("data.aws_region"), Vec::<String>::new());
        assert_eq!(in_text("var[0]"), Vec::<String>::new());
        assert_eq!(in_text("\"aws_vpc.main.id\""), Vec::<String>::new());
    }

    #[test]
    fn finds_references_wherever_an_expression_can_be() {
        assert_eq!(
            in_text(
                r#"merge(var.tags, {
                    Name = "web-${local.name}"
                    Zone = aws_subnet.all[local.zone].availability_zone
                    Ids  = [for s in aws_subnet.all : s.id if s.public == var.public]
                    Body = var.enabled ? data.aws_iam_policy_document.x.json : null
                })"#
            ),
            [
                "var.tags",
                "local.name",
                "aws_subnet.all",
                "local.zone",
                "aws_subnet.all",
                "s.id",
                "s.public",
                "var.public",
                "var.enabled",
                "data.aws_iam_policy_document.x",
            ]
        );
    }

    #[test]
    fn records_where_the_reference_is() {
        let text = "x = 1\ny = aws_vpc.main.id\n";
        let body: Body = text.parse().unwrap();
        assert_eq!(
            in_body(&body),
            [("aws_vpc.main".to_owned(), text.find("aws_vpc").unwrap())]
        );
    }

    #[test]
    fn json_strings_are_templates_or_bare_references() {
        assert_eq!(
            addresses(in_json_string("${aws_vpc.main.id}-${var.suffix}")),
            ["aws_vpc.main", "var.suffix"]
        );
        assert_eq!(
            addresses(in_json_string("aws_vpc.main.id")),
            Vec::<String>::new()
        );
        assert_eq!(
            addresses(in_json_string("${unterminated")),
            Vec::<String>::new()
        );
        assert_eq!(
            addresses(in_json_reference("aws_vpc.main")),
            ["aws_vpc.main"]
        );
        assert_eq!(
            addresses(in_json_reference("not a reference")),
            Vec::<String>::new()
        );
    }
}
