//! Regular expressions covering the XPath/SPARQL subset used in practice: literals, `.`,
//! classes, escapes (\d \w \s ...), anchors, groups, alternation, greedy/lazy quantifiers,
//! flags i/s/m/x/q, capture groups and back-references.
//!
//! Patterns are compiled to a Pike VM (Thompson NFA simulation with captures): matching
//! takes time linear in the text for any pattern, uses no recursion over the text, and
//! keeps Perl's leftmost-first semantics. Only patterns with back-references, which no
//! automaton can match, use a backtracker, and that one has a hard step budget.

#[derive(Debug, Clone)]
enum Node {
    Char(char),
    Any,
    Class(Vec<ClassItem>, bool), // items, negated
    Start,
    End,
    Group(Box<Node>, Option<usize>), // capture index
    Alt(Vec<Node>),
    Concat(Vec<Node>),
    Repeat(Box<Node>, usize, Option<usize>, bool), // min, max, greedy
    Backref(usize),
}

#[derive(Debug, Clone)]
enum ClassItem {
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

#[derive(Debug, Clone)]
enum Inst {
    Char(char),
    Any,
    Class(Vec<ClassItem>, bool),
    Start,
    End,
    /// Try the first branch before the second.
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    Match,
}

/// Limits that keep hostile patterns cheap: nesting depth while parsing, program size
/// (counted repeats are expanded), and the backtracker's step budget.
const MAX_DEPTH: usize = 200;
const MAX_PROGRAM: usize = 100_000;
const BACKTRACK_STEPS: usize = 100_000;

#[derive(Debug, Clone)]
/// A compiled XPath/XSD regular expression.
pub struct Regex {
    node: Node,
    /// The compiled program, or None when the pattern needs the backtracker.
    prog: Option<Vec<Inst>>,
    groups: usize,
    icase: bool,
    dotall: bool,
    multiline: bool,
    /// Prefilter: literal text every match starts with (`^abc...`, no `m` flag), and literal
    /// runs every match contains; texts without them are rejected before the automaton runs.
    prefix: Option<String>,
    required: Vec<String>,
}

/// Literal runs of the pattern's top-level sequence, and whether the first is anchored at
/// the start of the text.
fn literals(node: &Node) -> (bool, Vec<String>) {
    let items: &[Node] = match node {
        Node::Concat(v) => v,
        other => std::slice::from_ref(other),
    };
    let anchored = matches!(items.first(), Some(Node::Start));
    let mut runs = Vec::new();
    let mut run = String::new();
    let mut first_run_anchored = anchored;
    for (k, n) in items.iter().enumerate() {
        match n {
            Node::Char(c) => run.push(*c),
            Node::Start if k == 0 => {}
            _ => {
                if !run.is_empty() {
                    runs.push(std::mem::take(&mut run));
                } else if runs.is_empty() {
                    first_run_anchored = false;
                }
            }
        }
    }
    if !run.is_empty() {
        runs.push(run);
    }
    (first_run_anchored && !runs.is_empty(), runs)
}

struct P<'a> {
    c: Vec<char>,
    i: usize,
    depth: usize,
    groups: usize,
    extended: bool,
    _s: &'a str,
}

impl Regex {
    /// Compile a pattern with SPARQL's flags (`i s m x q`).
    pub fn new(pattern: &str, flags: &str) -> Result<Regex, String> {
        let mut p = P {
            c: pattern.chars().collect(),
            i: 0,
            depth: 0,
            groups: 0,
            extended: flags.contains('x'),
            _s: pattern,
        };
        let node = if flags.contains('q') {
            Node::Concat(pattern.chars().map(Node::Char).collect()) // literal pattern
        } else {
            let node = p.alt()?;
            if p.i < p.c.len() {
                return Err(format!("regex: unexpected '{}'", p.c[p.i]));
            }
            node
        };
        let prog = if has_backref(&node) {
            None
        } else {
            let mut prog = vec![Inst::Save(0)];
            compile(&node, &mut prog)?;
            prog.push(Inst::Save(1));
            prog.push(Inst::Match);
            Some(prog)
        };
        let icase = flags.contains('i');
        let multiline = flags.contains('m');
        // case-insensitive matching would need case folding: no prefilter then
        let (anchored, runs) = if icase {
            (false, Vec::new())
        } else {
            literals(&node)
        };
        let prefix = if anchored && !multiline {
            runs.first().cloned()
        } else {
            None
        };
        Ok(Regex {
            node,
            prog,
            groups: p.groups,
            icase,
            dotall: flags.contains('s'),
            multiline,
            prefix,
            required: runs,
        })
    }

    /// Whether the text matches somewhere.
    pub fn is_match(&self, text: &str) -> bool {
        if self
            .prefix
            .as_ref()
            .is_some_and(|p| !text.starts_with(p.as_str()))
            || self.required.iter().any(|r| !text.contains(r.as_str()))
        {
            return false;
        }
        self.find_at(&text.chars().collect::<Vec<_>>(), 0).is_some()
    }

    /// Returns (start, end, captures) in char indices.
    fn find_at(
        &self,
        t: &[char],
        from: usize,
    ) -> Option<(usize, usize, Vec<Option<(usize, usize)>>)> {
        if let Some(prog) = &self.prog {
            return self.pike(prog, t, from);
        }
        let mut steps = 0usize;
        for start in from..=t.len() {
            let mut caps = vec![None; self.groups + 1];
            if let Some(end) = self.m(
                &self.node,
                t,
                start,
                &mut caps,
                &mut steps,
                &mut |_, e, _| Some(e),
            ) {
                return Some((start, end, caps));
            }
            if steps > BACKTRACK_STEPS {
                return None;
            }
        }
        None
    }

    fn step_ok(&self, inst: &Inst, t: &[char], i: usize) -> bool {
        match inst {
            Inst::Char(c) => i < t.len() && self.eq(t[i], *c),
            Inst::Any => i < t.len() && (self.dotall || t[i] != '\n'),
            Inst::Class(items, neg) => i < t.len() && self.class_match(items, *neg, t[i]),
            _ => false,
        }
    }

    /// Follow empty transitions from `pc` at position `i`, appending the threads that wait
    /// on a character (or match) to `list` in priority order. Iterative: the explicit stack
    /// restores capture slots on the way back, like the recursive formulation.
    fn add_thread(
        &self,
        prog: &[Inst],
        list: &mut Vec<(usize, Vec<usize>)>,
        seen: &mut [usize],
        epoch: usize,
        pc: usize,
        caps: &mut Vec<usize>,
        t: &[char],
        i: usize,
    ) {
        enum Job {
            Visit(usize),
            Restore(usize, usize),
        }
        let mut stack = vec![Job::Visit(pc)];
        while let Some(job) = stack.pop() {
            let pc = match job {
                Job::Restore(slot, old) => {
                    caps[slot] = old;
                    continue;
                }
                Job::Visit(pc) => pc,
            };
            if seen[pc] == epoch {
                continue;
            }
            seen[pc] = epoch;
            match &prog[pc] {
                Inst::Jmp(x) => stack.push(Job::Visit(*x)),
                Inst::Split(x, y) => {
                    stack.push(Job::Visit(*y));
                    stack.push(Job::Visit(*x));
                }
                Inst::Save(slot) => {
                    stack.push(Job::Restore(*slot, caps[*slot]));
                    caps[*slot] = i;
                    stack.push(Job::Visit(pc + 1));
                }
                Inst::Start => {
                    if i == 0 || (self.multiline && t[i - 1] == '\n') {
                        stack.push(Job::Visit(pc + 1));
                    }
                }
                Inst::End => {
                    if i == t.len() || (self.multiline && t[i] == '\n') {
                        stack.push(Job::Visit(pc + 1));
                    }
                }
                _ => list.push((pc, caps.clone())),
            }
        }
    }

    fn pike(
        &self,
        prog: &[Inst],
        t: &[char],
        from: usize,
    ) -> Option<(usize, usize, Vec<Option<(usize, usize)>>)> {
        const UNSET: usize = usize::MAX;
        let slots = 2 * (self.groups + 1);
        let mut seen = vec![0usize; prog.len()];
        let mut epoch = 1; // marks the closure that built `clist`
        let mut clist: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut nlist: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut matched: Option<Vec<usize>> = None;
        let mut i = from;
        loop {
            if matched.is_none() {
                // a new attempt starting here has the lowest priority
                let mut caps = vec![UNSET; slots];
                self.add_thread(prog, &mut clist, &mut seen, epoch, 0, &mut caps, t, i);
            }
            if clist.is_empty() {
                if matched.is_some() || i >= t.len() {
                    break;
                }
                i += 1; // nothing alive: the next attempt starts at the next position
                epoch += 1;
                continue;
            }
            epoch += 1;
            for (pc, caps) in clist.iter_mut() {
                match &prog[*pc] {
                    Inst::Match => {
                        matched = Some(caps.clone());
                        break; // lower-priority threads can no longer win
                    }
                    inst if self.step_ok(inst, t, i) => {
                        self.add_thread(prog, &mut nlist, &mut seen, epoch, *pc + 1, caps, t, i + 1)
                    }
                    _ => {}
                }
            }
            std::mem::swap(&mut clist, &mut nlist);
            nlist.clear();
            if i >= t.len() {
                break;
            }
            i += 1;
        }
        let caps = matched?;
        let pair = |g: usize| {
            (caps[2 * g] != UNSET && caps[2 * g + 1] != UNSET)
                .then(|| (caps[2 * g], caps[2 * g + 1]))
        };
        let (s, e) = pair(0)?;
        Some((
            s,
            e,
            (0..=self.groups)
                .map(|g| if g == 0 { None } else { pair(g) })
                .collect(),
        ))
    }

    /// REPLACE: every match substituted (with `$1` group references).
    pub fn replace_all(&self, text: &str, replacement: &str) -> String {
        let t: Vec<char> = text.chars().collect();
        let rep: Vec<char> = replacement.chars().collect();
        let mut out = String::new();
        let mut pos = 0;
        while pos <= t.len() {
            let Some((s, e, caps)) = self.find_at(&t, pos) else {
                break;
            };
            out.extend(&t[pos..s]);
            let mut k = 0;
            while k < rep.len() {
                if rep[k] == '\\' && k + 1 < rep.len() {
                    out.push(rep[k + 1]);
                    k += 2;
                } else if rep[k] == '$' && k + 1 < rep.len() && rep[k + 1].is_ascii_digit() {
                    let g = rep[k + 1].to_digit(10).unwrap() as usize;
                    if g == 0 {
                        out.extend(&t[s..e]);
                    } else if let Some(Some((a, b))) = caps.get(g) {
                        out.extend(&t[*a..*b]);
                    }
                    k += 2;
                } else {
                    out.push(rep[k]);
                    k += 1;
                }
            }
            if e == s {
                if s < t.len() {
                    out.push(t[s]);
                }
                pos = s + 1;
            } else {
                pos = e;
            }
        }
        if pos < t.len() {
            out.extend(&t[pos..]);
        }
        out
    }

    fn eq(&self, a: char, b: char) -> bool {
        a == b || (self.icase && (a.to_lowercase().eq(b.to_lowercase())))
    }

    fn class_match(&self, items: &[ClassItem], neg: bool, c: char) -> bool {
        let hit = items.iter().any(|it| match it {
            ClassItem::Range(a, b) => {
                (*a <= c && c <= *b)
                    || (self.icase
                        && c.to_lowercase()
                            .chain(c.to_uppercase())
                            .any(|x| *a <= x && x <= *b))
            }
            ClassItem::Digit(n) => c.is_ascii_digit() != *n,
            ClassItem::Word(n) => (c.is_alphanumeric() || c == '_') != *n,
            ClassItem::Space(n) => c.is_whitespace() != *n,
        });
        hit != neg
    }

    /// Continuation-passing matcher: `k` is called with the end position.
    fn m(
        &self,
        n: &Node,
        t: &[char],
        i: usize,
        caps: &mut Vec<Option<(usize, usize)>>,
        steps: &mut usize,
        k: &mut dyn FnMut(&Self, usize, &mut Vec<Option<(usize, usize)>>) -> Option<usize>,
    ) -> Option<usize> {
        *steps += 1;
        if *steps > BACKTRACK_STEPS {
            return None; // pathological backtracking guard
        }
        match n {
            Node::Char(c) => (i < t.len() && self.eq(t[i], *c))
                .then(|| k(self, i + 1, caps))
                .flatten(),
            Node::Any => (i < t.len() && (self.dotall || t[i] != '\n'))
                .then(|| k(self, i + 1, caps))
                .flatten(),
            Node::Class(items, neg) => (i < t.len() && self.class_match(items, *neg, t[i]))
                .then(|| k(self, i + 1, caps))
                .flatten(),
            Node::Start => (i == 0 || (self.multiline && t[i - 1] == '\n'))
                .then(|| k(self, i, caps))
                .flatten(),
            Node::End => (i == t.len() || (self.multiline && t[i] == '\n'))
                .then(|| k(self, i, caps))
                .flatten(),
            Node::Backref(g) => {
                let (a, b) = caps.get(*g).copied().flatten()?;
                let len = b - a;
                if i + len <= t.len() && (0..len).all(|j| self.eq(t[i + j], t[a + j])) {
                    k(self, i + len, caps)
                } else {
                    None
                }
            }
            Node::Group(inner, idx) => {
                let idx = *idx;
                self.m(inner, t, i, caps, steps, &mut |s: &Self,
                                                       e,
                                                       caps: &mut Vec<
                    Option<(usize, usize)>,
                >| {
                    if let Some(g) = idx {
                        let saved = caps[g];
                        caps[g] = Some((i, e));
                        let r = k(s, e, caps);
                        if r.is_none() {
                            caps[g] = saved;
                        }
                        r
                    } else {
                        k(s, e, caps)
                    }
                })
            }
            Node::Alt(alts) => {
                for a in alts {
                    if let Some(r) = self.m(a, t, i, caps, steps, k) {
                        return Some(r);
                    }
                }
                None
            }
            Node::Concat(items) => self.concat(items, t, i, caps, steps, k),
            Node::Repeat(inner, min, max, greedy) => self.repeat(
                inner,
                *min,
                *max,
                *greedy,
                0,
                t,
                i,
                usize::MAX,
                caps,
                steps,
                k,
            ),
        }
    }

    fn concat(
        &self,
        items: &[Node],
        t: &[char],
        i: usize,
        caps: &mut Vec<Option<(usize, usize)>>,
        steps: &mut usize,
        k: &mut dyn FnMut(&Self, usize, &mut Vec<Option<(usize, usize)>>) -> Option<usize>,
    ) -> Option<usize> {
        match items.split_first() {
            None => k(self, i, caps),
            Some((first, rest)) => {
                let budget = std::cell::Cell::new(*steps);
                let r = self.m(first, t, i, caps, steps, &mut |s: &Self,
                                                               e,
                                                               caps: &mut Vec<
                    Option<(usize, usize)>,
                >| {
                    let mut st = budget.get();
                    let r = s.concat(rest, t, e, caps, &mut st, k);
                    budget.set(st);
                    r
                });
                *steps = (*steps).max(budget.get());
                r
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repeat(
        &self,
        inner: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
        count: usize,
        t: &[char],
        i: usize,
        last: usize,
        caps: &mut Vec<Option<(usize, usize)>>,
        steps: &mut usize,
        k: &mut dyn FnMut(&Self, usize, &mut Vec<Option<(usize, usize)>>) -> Option<usize>,
    ) -> Option<usize> {
        let can_more = max.is_none_or(|m| count < m) && last != i; // stop on empty iterations
        let more = |s: &Self,
                    caps: &mut Vec<Option<(usize, usize)>>,
                    steps: &mut usize,
                    k: &mut dyn FnMut(
            &Self,
            usize,
            &mut Vec<Option<(usize, usize)>>,
        ) -> Option<usize>| {
            let budget = std::cell::Cell::new(*steps);
            let r = s.m(inner, t, i, caps, steps, &mut |s2: &Self,
                                                        e,
                                                        caps: &mut Vec<
                Option<(usize, usize)>,
            >| {
                let mut st = budget.get();
                let r = s2.repeat(
                    inner,
                    min,
                    max,
                    greedy,
                    count + 1,
                    t,
                    e,
                    i,
                    caps,
                    &mut st,
                    k,
                );
                budget.set(st);
                r
            });
            *steps = (*steps).max(budget.get());
            r
        };
        if count < min {
            return more(self, caps, steps, k);
        }
        if greedy {
            if can_more {
                if let Some(r) = more(self, caps, steps, k) {
                    return Some(r);
                }
            }
            k(self, i, caps)
        } else {
            if let Some(r) = k(self, i, caps) {
                return Some(r);
            }
            if can_more {
                more(self, caps, steps, k)
            } else {
                None
            }
        }
    }
}

fn has_backref(n: &Node) -> bool {
    match n {
        Node::Backref(_) => true,
        Node::Group(x, _) | Node::Repeat(x, ..) => has_backref(x),
        Node::Alt(xs) | Node::Concat(xs) => xs.iter().any(has_backref),
        _ => false,
    }
}

fn compile(n: &Node, prog: &mut Vec<Inst>) -> Result<(), String> {
    if prog.len() > MAX_PROGRAM {
        return Err("regex: pattern too large".into());
    }
    match n {
        Node::Char(c) => prog.push(Inst::Char(*c)),
        Node::Any => prog.push(Inst::Any),
        Node::Class(items, neg) => prog.push(Inst::Class(items.clone(), *neg)),
        Node::Start => prog.push(Inst::Start),
        Node::End => prog.push(Inst::End),
        Node::Backref(_) => unreachable!("patterns with back-references use the backtracker"),
        Node::Group(inner, None) => compile(inner, prog)?,
        Node::Group(inner, Some(g)) => {
            prog.push(Inst::Save(2 * g));
            compile(inner, prog)?;
            prog.push(Inst::Save(2 * g + 1));
        }
        Node::Concat(items) => {
            for it in items {
                compile(it, prog)?;
            }
        }
        Node::Alt(alts) => {
            let mut jumps = Vec::new();
            for (k, a) in alts.iter().enumerate() {
                if k + 1 < alts.len() {
                    let split = prog.len();
                    prog.push(Inst::Split(split + 1, 0));
                    compile(a, prog)?;
                    jumps.push(prog.len());
                    prog.push(Inst::Jmp(0));
                    let next = prog.len();
                    prog[split] = Inst::Split(split + 1, next);
                } else {
                    compile(a, prog)?;
                }
            }
            let end = prog.len();
            for j in jumps {
                prog[j] = Inst::Jmp(end);
            }
        }
        Node::Repeat(inner, min, max, greedy) => {
            let split = |body: usize, out: usize| {
                if *greedy {
                    Inst::Split(body, out)
                } else {
                    Inst::Split(out, body)
                }
            };
            for _ in 0..*min {
                compile(inner, prog)?;
            }
            match max {
                None => {
                    // The back edge is a split too: when an iteration matched the empty
                    // string the loop head is already visited at this position, and the
                    // thread leaves the loop there - Perl's rule for empty iterations.
                    let top = prog.len();
                    prog.push(Inst::Jmp(0));
                    compile(inner, prog)?;
                    let back = prog.len();
                    prog.push(Inst::Jmp(0));
                    let out = prog.len();
                    prog[top] = split(top + 1, out);
                    prog[back] = split(top, out);
                }
                Some(max) => {
                    let mut holes = Vec::new();
                    for _ in *min..*max {
                        holes.push(prog.len());
                        prog.push(Inst::Jmp(0));
                        compile(inner, prog)?;
                        if prog.len() > MAX_PROGRAM {
                            return Err("regex: pattern too large".into());
                        }
                    }
                    let out = prog.len();
                    for h in holes {
                        prog[h] = split(h + 1, out);
                    }
                }
            }
        }
    }
    Ok(())
}

impl P<'_> {
    fn peek(&mut self) -> Option<char> {
        if self.extended {
            while self.i < self.c.len() && self.c[self.i].is_whitespace() {
                self.i += 1;
            }
        }
        self.c.get(self.i).copied()
    }

    fn alt(&mut self) -> Result<Node, String> {
        let mut alts = vec![self.concat()?];
        while self.peek() == Some('|') {
            self.i += 1;
            alts.push(self.concat()?);
        }
        Ok(if alts.len() == 1 {
            alts.pop().unwrap()
        } else {
            Node::Alt(alts)
        })
    }

    fn concat(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            items.push(self.quant(atom)?);
        }
        Ok(Node::Concat(items))
    }

    fn quant(&mut self, atom: Node) -> Result<Node, String> {
        let (min, max) = match self.peek() {
            Some('*') => (0, None),
            Some('+') => (1, None),
            Some('?') => (0, Some(1)),
            Some('{') => {
                let save = self.i;
                self.i += 1;
                let num = |p: &mut Self| {
                    let s = p.i;
                    while p.i < p.c.len() && p.c[p.i].is_ascii_digit() {
                        p.i += 1;
                    }
                    p.c[s..p.i].iter().collect::<String>().parse::<usize>().ok()
                };
                let Some(lo) = num(self) else {
                    self.i = save;
                    return Ok(atom);
                };
                let hi = if self.c.get(self.i) == Some(&',') {
                    self.i += 1;
                    num(self)
                } else {
                    Some(lo)
                };
                if self.c.get(self.i) != Some(&'}') {
                    return Err("regex: bad {n,m}".into());
                }
                if hi.is_some_and(|h| h < lo) || lo > 10_000 || hi.is_some_and(|h| h > 10_000) {
                    return Err("regex: bad repetition bounds".into());
                }
                (lo, hi)
            }
            _ => return Ok(atom),
        };
        self.i += 1;
        let greedy = if self.c.get(self.i) == Some(&'?') {
            self.i += 1;
            false
        } else {
            true
        };
        Ok(Node::Repeat(Box::new(atom), min, max, greedy))
    }

    fn escape_class(c: char) -> Option<ClassItem> {
        Some(match c {
            'd' => ClassItem::Digit(false),
            'D' => ClassItem::Digit(true),
            'w' => ClassItem::Word(false),
            'W' => ClassItem::Word(true),
            's' => ClassItem::Space(false),
            'S' => ClassItem::Space(true),
            _ => return None,
        })
    }

    fn escaped_char(c: char) -> char {
        match c {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            c => c,
        }
    }

    fn atom(&mut self) -> Result<Node, String> {
        let c = self.peek().ok_or("regex: unexpected end")?;
        self.i += 1;
        Ok(match c {
            '.' => Node::Any,
            '^' => Node::Start,
            '$' => Node::End,
            '(' => {
                let idx =
                    if self.c.get(self.i) == Some(&'?') && self.c.get(self.i + 1) == Some(&':') {
                        self.i += 2;
                        None
                    } else {
                        self.groups += 1;
                        Some(self.groups)
                    };
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err("regex: groups nested too deeply".into());
                }
                let inner = self.alt()?;
                self.depth -= 1;
                if self.peek() != Some(')') {
                    return Err("regex: missing ')'".into());
                }
                self.i += 1;
                Node::Group(Box::new(inner), idx)
            }
            '[' => self.class()?,
            '\\' => {
                let e = *self.c.get(self.i).ok_or("regex: trailing backslash")?;
                self.i += 1;
                if let Some(ci) = Self::escape_class(e) {
                    Node::Class(vec![ci], false)
                } else if e.is_ascii_digit() && e != '0' {
                    Node::Backref(e.to_digit(10).unwrap() as usize)
                } else {
                    Node::Char(Self::escaped_char(e))
                }
            }
            c => Node::Char(c),
        })
    }

    fn class(&mut self) -> Result<Node, String> {
        let mut neg = false;
        if self.c.get(self.i) == Some(&'^') {
            neg = true;
            self.i += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let c = *self.c.get(self.i).ok_or("regex: unterminated class")?;
            self.i += 1;
            if c == ']' && !first {
                break;
            }
            first = false;
            let lo = if c == '\\' {
                let e = *self.c.get(self.i).ok_or("regex: bad escape")?;
                self.i += 1;
                if let Some(ci) = Self::escape_class(e) {
                    items.push(ci);
                    continue;
                }
                Self::escaped_char(e)
            } else {
                c
            };
            if self.c.get(self.i) == Some(&'-') && self.c.get(self.i + 1).is_some_and(|&n| n != ']')
            {
                self.i += 1;
                let mut hi = *self.c.get(self.i).ok_or("regex: unterminated class")?;
                self.i += 1;
                if hi == '\\' {
                    hi = Self::escaped_char(*self.c.get(self.i).ok_or("regex: bad escape")?);
                    self.i += 1;
                }
                items.push(ClassItem::Range(lo, hi));
            } else {
                items.push(ClassItem::Range(lo, lo));
            }
        }
        Ok(Node::Class(items, neg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert!(Regex::new("^ab+c$", "").unwrap().is_match("abbbc"));
        assert!(!Regex::new("^ab+c$", "").unwrap().is_match("ac"));
        assert!(Regex::new("HELLO", "i").unwrap().is_match("say hello"));
        assert!(Regex::new("[a-c]{2,3}\\d", "").unwrap().is_match("xxbca7"));
        assert!(Regex::new("(foo|bar)baz", "").unwrap().is_match("barbaz"));
        assert_eq!(
            Regex::new("(\\w+)@(\\w+)", "")
                .unwrap()
                .replace_all("me@host", "$2 at $1"),
            "host at me"
        );
    }

    #[test]
    fn leftmost_first_semantics() {
        let r = |p: &str, t: &str| Regex::new(p, "").unwrap().replace_all(t, "<$0>");
        assert_eq!(r("a|ab", "ab"), "<a>b");
        assert_eq!(r("ab|a", "ab"), "<ab>");
        assert_eq!(r("a+?", "aaa"), "<a><a><a>");
        assert_eq!(r("(a*)*b", "aab"), "<aab>");
        assert_eq!(r("$", "ab"), "ab<>");
        assert!(Regex::new("^b", "m").unwrap().is_match("a\nb"));
        assert!(Regex::new("(a)\\1", "").unwrap().is_match("xaa")); // back-reference
        assert_eq!(
            Regex::new("a.c", "q").unwrap().replace_all("abc a.c", "_"),
            "abc _"
        );
    }

    #[test]
    fn hostile_patterns_are_cheap() {
        let t = std::time::Instant::now();
        let text = "a".repeat(20_000) + "!";
        assert!(!Regex::new("^(a+)+$", "").unwrap().is_match(&text));
        assert!(!Regex::new("(a|aa)*c", "").unwrap().is_match(&text));
        assert!(
            Regex::new("x.*y", "")
                .unwrap()
                .is_match(&("x".repeat(200_000) + "y"))
        );
        assert!(t.elapsed().as_secs() < 2);
        assert!(Regex::new(&"(".repeat(1000), "").is_err());
        assert!(Regex::new("a{100000}", "").is_err());
        assert!(Regex::new("[a-", "").is_err());
    }
}
