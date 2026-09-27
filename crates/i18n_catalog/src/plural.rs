//! Evaluates the `Plural-Forms` header of a PO file: the subset of C expressions
//! that gettext accepts, over the single variable `n`.

use anyhow::{Context as _, Result, anyhow, bail};

/// Deeply nested parentheses in a hand-edited file must not overflow the stack.
const MAX_DEPTH: usize = 64;

/// A chain of binary operators (`n+n+n+…`) is parsed by a loop into a tree as deep as
/// the chain is long, which `MAX_DEPTH` does not see; evaluating or dropping such a
/// tree recurses once per operator. Real rules have well under a hundred tokens.
const MAX_TOKENS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluralRule {
    plural_count: usize,
    expression: Expression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Expression {
    Count,
    Number(u64),
    Not(Box<Expression>),
    Binary(BinaryOperator, Box<Expression>, Box<Expression>),
    Conditional(Box<Expression>, Box<Expression>, Box<Expression>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinaryOperator {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

impl PluralRule {
    /// The rule English uses, and the fallback for a missing or broken header.
    pub fn english() -> Self {
        Self {
            plural_count: 2,
            expression: Expression::Binary(
                BinaryOperator::NotEqual,
                Box::new(Expression::Count),
                Box::new(Expression::Number(1)),
            ),
        }
    }

    /// Parses a header value such as `nplurals=2; plural=(n != 1);`.
    pub fn parse(header_value: &str) -> Result<Self> {
        let mut plural_count = None;
        let mut expression = None;
        for part in header_value.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (name, value) = part
                .split_once('=')
                .with_context(|| format!("expected `name=value` in {part:?}"))?;
            match name.trim() {
                "nplurals" => {
                    let count: usize = value
                        .trim()
                        .parse()
                        .with_context(|| format!("nplurals is not a number: {value:?}"))?;
                    if count == 0 {
                        bail!("nplurals must be at least 1");
                    }
                    plural_count = Some(count);
                }
                "plural" => expression = Some(Parser::parse(value)?),
                _ => {}
            }
        }
        Ok(Self {
            plural_count: plural_count.context("nplurals is missing")?,
            expression: expression.context("plural is missing")?,
        })
    }

    pub fn plural_count(&self) -> usize {
        self.plural_count
    }

    /// The index of the form to use for `count`. Evaluation errors such as division by
    /// zero, and indices beyond `nplurals`, select the first form.
    pub fn form_index(&self, count: u64) -> usize {
        match evaluate(&self.expression, count) {
            Some(index) => usize::try_from(index)
                .ok()
                .filter(|index| *index < self.plural_count)
                .unwrap_or(0),
            None => 0,
        }
    }
}

fn evaluate(expression: &Expression, count: u64) -> Option<u64> {
    Some(match expression {
        Expression::Count => count,
        Expression::Number(value) => *value,
        Expression::Not(operand) => u64::from(evaluate(operand, count)? == 0),
        Expression::Conditional(condition, if_true, if_false) => {
            if evaluate(condition, count)? != 0 {
                evaluate(if_true, count)?
            } else {
                evaluate(if_false, count)?
            }
        }
        Expression::Binary(BinaryOperator::Or, left, right) => {
            u64::from(evaluate(left, count)? != 0 || evaluate(right, count)? != 0)
        }
        Expression::Binary(BinaryOperator::And, left, right) => {
            u64::from(evaluate(left, count)? != 0 && evaluate(right, count)? != 0)
        }
        Expression::Binary(operator, left, right) => {
            let left = evaluate(left, count)?;
            let right = evaluate(right, count)?;
            match operator {
                BinaryOperator::Equal => u64::from(left == right),
                BinaryOperator::NotEqual => u64::from(left != right),
                BinaryOperator::Less => u64::from(left < right),
                BinaryOperator::LessOrEqual => u64::from(left <= right),
                BinaryOperator::Greater => u64::from(left > right),
                BinaryOperator::GreaterOrEqual => u64::from(left >= right),
                // gettext evaluates in `unsigned long`, which wraps.
                BinaryOperator::Add => left.wrapping_add(right),
                BinaryOperator::Subtract => left.wrapping_sub(right),
                BinaryOperator::Multiply => left.wrapping_mul(right),
                BinaryOperator::Divide => left.checked_div(right)?,
                BinaryOperator::Remainder => left.checked_rem(right)?,
                BinaryOperator::Or | BinaryOperator::And => return None,
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Count,
    Number(u64),
    Question,
    Colon,
    OpenParenthesis,
    CloseParenthesis,
    Not,
    Operator(BinaryOperator),
}

fn tokenize(source: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut characters = source.char_indices().peekable();
    while let Some((position, character)) = characters.next() {
        let followed_by = |expected: char, characters: &mut std::iter::Peekable<_>| {
            let matches = matches!(characters.peek(), Some((_, next)) if *next == expected);
            if matches {
                characters.next();
            }
            matches
        };
        let token = match character {
            ' ' | '\t' | '\r' | '\n' => continue,
            'n' => Token::Count,
            '0'..='9' => {
                let mut value = u64::from(character as u8 - b'0');
                while let Some((_, digit @ '0'..='9')) = characters.peek().copied() {
                    characters.next();
                    value = value
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(u64::from(digit as u8 - b'0')))
                        .context("number is too large")?;
                }
                Token::Number(value)
            }
            '?' => Token::Question,
            ':' => Token::Colon,
            '(' => Token::OpenParenthesis,
            ')' => Token::CloseParenthesis,
            '+' => Token::Operator(BinaryOperator::Add),
            '-' => Token::Operator(BinaryOperator::Subtract),
            '*' => Token::Operator(BinaryOperator::Multiply),
            '/' => Token::Operator(BinaryOperator::Divide),
            '%' => Token::Operator(BinaryOperator::Remainder),
            '!' if followed_by('=', &mut characters) => Token::Operator(BinaryOperator::NotEqual),
            '!' => Token::Not,
            '=' if followed_by('=', &mut characters) => Token::Operator(BinaryOperator::Equal),
            '<' if followed_by('=', &mut characters) => {
                Token::Operator(BinaryOperator::LessOrEqual)
            }
            '<' => Token::Operator(BinaryOperator::Less),
            '>' if followed_by('=', &mut characters) => {
                Token::Operator(BinaryOperator::GreaterOrEqual)
            }
            '>' => Token::Operator(BinaryOperator::Greater),
            '|' if followed_by('|', &mut characters) => Token::Operator(BinaryOperator::Or),
            '&' if followed_by('&', &mut characters) => Token::Operator(BinaryOperator::And),
            _ => bail!("unexpected {character:?} at offset {position} in the plural expression"),
        };
        tokens.push(token);
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    depth: usize,
}

impl Parser {
    fn parse(source: &str) -> Result<Expression> {
        let tokens = tokenize(source)?;
        if tokens.len() > MAX_TOKENS {
            bail!("the plural expression is too long");
        }
        let mut parser = Parser {
            tokens,
            position: 0,
            depth: 0,
        };
        let expression = parser.conditional()?;
        if parser.position != parser.tokens.len() {
            bail!("unexpected text after the plural expression");
        }
        Ok(expression)
    }

    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.position).copied()
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.peek();
        if token.is_some() {
            self.position += 1;
        }
        token
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            bail!("the plural expression is nested too deeply");
        }
        Ok(())
    }

    fn conditional(&mut self) -> Result<Expression> {
        self.enter()?;
        let condition = self.binary(0)?;
        let expression = if self.peek() == Some(Token::Question) {
            self.advance();
            let if_true = self.conditional()?;
            if self.advance() != Some(Token::Colon) {
                bail!("expected `:` in the plural expression");
            }
            let if_false = self.conditional()?;
            Expression::Conditional(Box::new(condition), Box::new(if_true), Box::new(if_false))
        } else {
            condition
        };
        self.depth -= 1;
        Ok(expression)
    }

    fn binary(&mut self, minimum_precedence: u8) -> Result<Expression> {
        let mut left = self.unary()?;
        while let Some(Token::Operator(operator)) = self.peek() {
            let precedence = precedence(operator);
            if precedence < minimum_precedence {
                break;
            }
            self.advance();
            let right = self.binary(precedence + 1)?;
            left = Expression::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expression> {
        match self.advance() {
            Some(Token::Not) => {
                self.enter()?;
                let operand = self.unary()?;
                self.depth -= 1;
                Ok(Expression::Not(Box::new(operand)))
            }
            Some(Token::Count) => Ok(Expression::Count),
            Some(Token::Number(value)) => Ok(Expression::Number(value)),
            Some(Token::OpenParenthesis) => {
                let inner = self.conditional()?;
                if self.advance() != Some(Token::CloseParenthesis) {
                    bail!("expected `)` in the plural expression");
                }
                Ok(inner)
            }
            Some(token) => Err(anyhow!("unexpected {token:?} in the plural expression")),
            None => Err(anyhow!("the plural expression ends too early")),
        }
    }
}

fn precedence(operator: BinaryOperator) -> u8 {
    match operator {
        BinaryOperator::Or => 0,
        BinaryOperator::And => 1,
        BinaryOperator::Equal | BinaryOperator::NotEqual => 2,
        BinaryOperator::Less
        | BinaryOperator::LessOrEqual
        | BinaryOperator::Greater
        | BinaryOperator::GreaterOrEqual => 3,
        BinaryOperator::Add | BinaryOperator::Subtract => 4,
        BinaryOperator::Multiply | BinaryOperator::Divide | BinaryOperator::Remainder => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forms(header: &str, counts: &[u64]) -> Vec<usize> {
        let rule = PluralRule::parse(header).expect("the header should parse");
        counts.iter().map(|count| rule.form_index(*count)).collect()
    }

    #[test]
    fn japanese() {
        assert_eq!(
            forms("nplurals=1; plural=0;", &[0, 1, 2, 100]),
            [0, 0, 0, 0]
        );
    }

    #[test]
    fn english() {
        assert_eq!(
            forms("nplurals=2; plural=(n != 1);", &[0, 1, 2, 11]),
            [1, 0, 1, 1]
        );
        let rule = PluralRule::english();
        assert_eq!([0, 1, 2].map(|count| rule.form_index(count)), [1, 0, 1]);
    }

    #[test]
    fn french() {
        assert_eq!(
            forms("nplurals=2; plural=(n > 1);", &[0, 1, 2, 100]),
            [0, 0, 1, 1]
        );
    }

    #[test]
    fn russian() {
        let header = "nplurals=3; plural=(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);";
        assert_eq!(
            forms(header, &[1, 2, 4, 5, 11, 12, 14, 21, 22, 25, 101, 111, 0]),
            [0, 1, 1, 2, 2, 2, 2, 0, 1, 2, 0, 2, 2]
        );
    }

    #[test]
    fn polish() {
        let header = "nplurals=3; plural=(n==1 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);";
        assert_eq!(
            forms(header, &[1, 2, 4, 5, 12, 14, 21, 22, 24, 25, 0]),
            [0, 1, 1, 2, 2, 2, 2, 1, 1, 2, 2]
        );
    }

    #[test]
    fn arabic() {
        let header = "nplurals=6; plural=(n==0 ? 0 : n==1 ? 1 : n==2 ? 2 : n%100>=3 && n%100<=10 ? 3 : n%100>=11 ? 4 : 5);";
        assert_eq!(
            forms(header, &[0, 1, 2, 3, 10, 11, 99, 100, 102, 103, 111]),
            [0, 1, 2, 3, 3, 4, 4, 5, 5, 3, 4]
        );
    }

    #[test]
    fn operators_and_precedence() {
        assert_eq!(forms("nplurals=9; plural=1 + 2 * 3;", &[0]), [7]);
        assert_eq!(forms("nplurals=9; plural=(1 + 2) * 2;", &[0]), [6]);
        assert_eq!(forms("nplurals=9; plural=8 - 3 - 2;", &[0]), [3]);
        assert_eq!(forms("nplurals=9; plural=8 / 2 / 2;", &[0]), [2]);
        assert_eq!(forms("nplurals=2; plural=!n;", &[0, 5]), [1, 0]);
        assert_eq!(forms("nplurals=2; plural=!!n;", &[0, 5]), [0, 1]);
        assert_eq!(forms("nplurals=2; plural=n || 0;", &[0, 3]), [0, 1]);
        assert_eq!(
            forms("nplurals=2; plural=n == 1 || n == 2 && 0;", &[1, 2]),
            [1, 0]
        );
        assert_eq!(forms("nplurals=2; plural=0 && n || 1;", &[1]), [1]);
        assert_eq!(forms("nplurals=2; plural=n <= 1;", &[1, 2]), [1, 0]);
        assert_eq!(forms("nplurals=2; plural=n >= 2 == 1;", &[1, 2]), [0, 1]);
        assert_eq!(
            forms("nplurals=3; plural=n ? n == 1 ? 1 : 2 : 0;", &[0, 1, 5]),
            [0, 1, 2]
        );
    }

    #[test]
    fn evaluation_errors_select_the_first_form() {
        assert_eq!(forms("nplurals=3; plural=2 / (n - 1);", &[1, 2]), [0, 2]);
        assert_eq!(forms("nplurals=3; plural=n % 0;", &[5]), [0]);
        assert_eq!(forms("nplurals=2; plural=n;", &[1, 2, 7]), [1, 0, 0]);
        assert_eq!(forms("nplurals=2; plural=0 - 1;", &[0]), [0]);
    }

    #[test]
    fn broken_headers_are_errors() {
        for header in [
            "",
            "nplurals=2;",
            "plural=n;",
            "nplurals=0; plural=0;",
            "nplurals=x; plural=0;",
            "nplurals=2; plural=(n != 1;",
            "nplurals=2; plural=n != ;",
            "nplurals=2; plural=n ? 1;",
            "nplurals=2; plural=n $ 1;",
            "nplurals=2; plural=n = 1;",
            "nplurals=2; plural=1 1;",
            "nplurals=2; plural=99999999999999999999999;",
        ] {
            assert!(
                PluralRule::parse(header).is_err(),
                "{header:?} should not parse"
            );
        }
    }

    #[test]
    fn deep_nesting_is_an_error_and_not_a_stack_overflow() {
        let header = format!(
            "nplurals=2; plural={}n{};",
            "(".repeat(10_000),
            ")".repeat(10_000)
        );
        assert!(PluralRule::parse(&header).is_err());
        let header = format!("nplurals=2; plural={}n;", "!".repeat(10_000));
        assert!(PluralRule::parse(&header).is_err());
        let header = format!("nplurals=2; plural={}0;", "n ? ".repeat(10_000));
        assert!(PluralRule::parse(&header).is_err());
    }

    #[test]
    fn long_operator_chains_are_an_error_and_not_a_stack_overflow() {
        for operator in ["+", "*", "==", "&&", "||", "%"] {
            let header = format!(
                "nplurals=2; plural=n{};",
                format!(" {operator} n").repeat(200_000)
            );
            assert!(PluralRule::parse(&header).is_err(), "{operator}");
        }
        // The longest rule in common use (Arabic) stays well inside the limit.
        let arabic = "nplurals=6; plural=(n==0 ? 0 : n==1 ? 1 : n==2 ? 2 : n%100>=3 && n%100<=10 ? 3 : n%100>=11 ? 4 : 5);";
        assert!(PluralRule::parse(arabic).is_ok());
        let long_but_fine = format!("nplurals=2; plural=n{} != 1;", " + 0".repeat(200));
        assert_eq!(forms(&long_but_fine, &[1, 2]), [0, 1]);
    }
}
