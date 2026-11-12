//! Small CSV reader with quoted-field support and line-numbered errors.
use std::fs;
use std::path::Path;

pub fn parse_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    fields.push(cur);
    fields
}

pub fn quote(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub struct Table {
    pub name: String,
    pub header: Vec<String>,
    pub rows: Vec<(usize, Vec<String>)>,
}

pub struct Row<'a> {
    table: &'a Table,
    line: usize,
    cells: &'a [String],
}

impl Table {
    pub fn read(path: &Path) -> Result<Table, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Table::parse(&path.display().to_string(), &text)
    }

    pub fn parse(name: &str, text: &str) -> Result<Table, String> {
        let mut lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l.trim_end_matches('\r')))
            .filter(|(_, l)| !l.trim().is_empty());
        let (_, head) = lines.next().ok_or_else(|| format!("{name}: empty file"))?;
        let header: Vec<String> = parse_line(head)
            .iter()
            .map(|h| h.trim().to_lowercase())
            .collect();
        // Cells are looked up by name, so a repeated column would quietly use
        // the first one and ignore the other.
        if let Some(h) = header
            .iter()
            .enumerate()
            .find_map(|(i, h)| header[..i].contains(h).then_some(h))
        {
            return Err(format!("{name}: column '{h}' appears more than once"));
        }
        let mut rows = Vec::new();
        for (n, l) in lines {
            let cells = parse_line(l);
            if cells.len() != header.len() {
                return Err(format!(
                    "{name}:{n}: expected {} columns, found {}",
                    header.len(),
                    cells.len()
                ));
            }
            rows.push((n, cells));
        }
        Ok(Table {
            name: name.to_string(),
            header,
            rows,
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = Row<'_>> {
        self.rows.iter().map(|(n, c)| Row {
            table: self,
            line: *n,
            cells: c,
        })
    }
}

impl Row<'_> {
    pub fn get(&self, col: &str) -> Result<&str, String> {
        let i = self
            .table
            .header
            .iter()
            .position(|h| h == col)
            .ok_or_else(|| format!("{}: missing column '{col}'", self.table.name))?;
        Ok(self.cells[i].trim())
    }

    pub fn num<T: std::str::FromStr>(&self, col: &str) -> Result<T, String> {
        let v = self.get(col)?;
        v.parse()
            .map_err(|_| self.err(&format!("bad value '{v}' in column '{col}'")))
    }

    pub fn err(&self, msg: &str) -> String {
        format!("{}:{}: {msg}", self.table.name, self.line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_quotes_and_commas() {
        assert_eq!(
            parse_line(r#"a,"b, c","say ""hi""""#),
            vec!["a", "b, c", "say \"hi\""]
        );
        assert_eq!(parse_line(&quote("x, \"y\"")), vec!["x, \"y\""]);
    }

    #[test]
    fn reports_line_numbers() {
        let e = Table::parse("t", "a,b\n1,2\n3\n").err().unwrap();
        assert!(e.contains("t:3"), "{e}");
    }

    #[test]
    fn refuses_a_repeated_column() {
        let e = Table::parse("t", "bid,ask,Bid\n1,2,3\n").err().unwrap();
        assert!(e.contains("column 'bid' appears more than once"), "{e}");
    }
}
