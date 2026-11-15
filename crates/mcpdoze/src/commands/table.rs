// SPDX-License-Identifier: GPL-3.0-or-later

//! A tiny plain-text table: left-aligned columns, two spaces apart.

/// Rows of cells under a header.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// New table with the given headers.
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|s| (*s).to_owned()).collect(),
            rows: Vec::new(),
        }
    }

    /// Adds a row (missing cells are empty, extra ones dropped).
    pub fn row(&mut self, cells: Vec<String>) {
        let mut cells = cells;
        cells.resize(self.headers.len(), String::new());
        self.rows.push(cells);
    }

    /// Renders to text, one line per row, no trailing spaces.
    pub fn render(&self) -> String {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for r in &self.rows {
            for (w, c) in widths.iter_mut().zip(r) {
                *w = (*w).max(c.chars().count());
            }
        }
        let mut out = String::new();
        let mut line = |cells: &[String]| {
            let mut s = String::new();
            for (i, (c, w)) in cells.iter().zip(&widths).enumerate() {
                if i > 0 {
                    s.push_str("  ");
                }
                s.push_str(c);
                s.push_str(&" ".repeat(w - c.chars().count()));
            }
            out.push_str(s.trim_end());
            out.push('\n');
        };
        line(&self.headers);
        for r in &self.rows {
            line(r);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_columns() {
        let mut t = Table::new(&["NAME", "STATE"]);
        t.row(vec!["a".into(), "running".into()]);
        t.row(vec!["longer".into()]);
        assert_eq!(t.render(), "NAME    STATE\na       running\nlonger\n");
    }

    #[test]
    fn empty_table_has_header_only() {
        assert_eq!(Table::new(&["X"]).render(), "X\n");
    }
}
