//! Rewrite/location templates: literal text with `$var` / `${var}`
//! substitutions, compiled once at config load.
//!
//! Variables: $uri (current decoded path), $args (current query), $host,
//! $method, $remote_addr. Regex captures ($1..$9) are a later iteration.

pub struct Template {
    parts: Vec<Part>,
}

enum Part {
    Lit(String),
    Var(Var),
}

#[derive(Clone, Copy)]
enum Var {
    Uri,
    Args,
    Host,
    Method,
    RemoteAddr,
}

pub struct Vars<'a> {
    pub uri: &'a [u8],
    pub args: &'a [u8],
    pub host: &'a [u8],
    pub method: &'a [u8],
    pub remote_addr: &'a str,
}

impl Template {
    pub fn compile(input: &str) -> anyhow::Result<Template> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut rest = input;

        while let Some(pos) = rest.find('$') {
            lit.push_str(&rest[..pos]);
            rest = &rest[pos + 1..];

            let (name, after) = if let Some(stripped) = rest.strip_prefix('{') {
                let end = stripped
                    .find('}')
                    .ok_or_else(|| anyhow::anyhow!("unterminated ${{...}} in \"{input}\""))?;
                (&stripped[..end], &stripped[end + 1..])
            } else {
                let end = rest
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            };

            let var = match name {
                "uri" => Var::Uri,
                "args" | "query" => Var::Args,
                "host" => Var::Host,
                "method" => Var::Method,
                "remote_addr" => Var::RemoteAddr,
                other => anyhow::bail!("unknown template variable \"${other}\" in \"{input}\""),
            };

            if !lit.is_empty() {
                parts.push(Part::Lit(std::mem::take(&mut lit)));
            }
            parts.push(Part::Var(var));
            rest = after;
        }
        lit.push_str(rest);
        if !lit.is_empty() {
            parts.push(Part::Lit(lit));
        }

        Ok(Template { parts })
    }

    pub fn render(&self, vars: &Vars<'_>) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        for part in &self.parts {
            match part {
                Part::Lit(s) => out.extend_from_slice(s.as_bytes()),
                Part::Var(Var::Uri) => out.extend_from_slice(vars.uri),
                Part::Var(Var::Args) => out.extend_from_slice(vars.args),
                Part::Var(Var::Host) => out.extend_from_slice(vars.host),
                Part::Var(Var::Method) => out.extend_from_slice(vars.method),
                Part::Var(Var::RemoteAddr) => out.extend_from_slice(vars.remote_addr.as_bytes()),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars<'static> {
        Vars {
            uri: b"/path",
            args: b"a=1",
            host: b"example.test",
            method: b"GET",
            remote_addr: "10.0.0.1",
        }
    }

    fn render(tpl: &str) -> String {
        String::from_utf8(Template::compile(tpl).unwrap().render(&vars())).unwrap()
    }

    #[test]
    fn pure_literal_passes_through() {
        assert_eq!(render("/static/index.html"), "/static/index.html");
        assert_eq!(render(""), "");
    }

    #[test]
    fn substitutes_all_known_variables() {
        assert_eq!(render("$uri"), "/path");
        assert_eq!(render("$args"), "a=1");
        assert_eq!(render("$host"), "example.test");
        assert_eq!(render("$method"), "GET");
        assert_eq!(render("$remote_addr"), "10.0.0.1");
    }

    #[test]
    fn query_is_an_alias_for_args() {
        assert_eq!(render("$query"), "a=1");
    }

    #[test]
    fn braced_form_and_adjacent_literals() {
        // `${uri}` lets a variable butt against following word characters.
        assert_eq!(render("${uri}_suffix"), "/path_suffix");
        assert_eq!(render("pre-$host-post"), "pre-example.test-post");
        assert_eq!(render("$uri?$args"), "/path?a=1");
    }

    #[test]
    fn unknown_variable_is_rejected() {
        assert!(Template::compile("$nope").is_err());
        assert!(Template::compile("${nope}").is_err());
    }

    #[test]
    fn unterminated_brace_is_rejected() {
        assert!(Template::compile("${uri").is_err());
    }
}
