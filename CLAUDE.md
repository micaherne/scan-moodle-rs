# Working with this repo

## Explain findings in plain language, not code-literate shorthand

The user is code-literate, but has not read through this codebase's internals the way an agent
does while working in it. When explaining what code does or why a bug happens, write for someone
in that position: don't name functions, variables, or types and assume that carries meaning on its
own ("`is_whole_dynamic_segment` only fires when..."), since that name means nothing without
having read the function it refers to. Describe the actual behaviour in plain terms first — what
happens, for what input, and why — and only mention a function/file name afterwards as a locator,
not as the explanation itself.

Bad (assumes the reader just read the code with you):

> The resolver already has an exception for exactly this shape, but it only fires when the marker
> fills the *entire* last segment (`is_whole_dynamic_segment` requires it to start with `{` and end
> with `}`). `{$file}.php` fuses the marker with a literal suffix, so it fails that check and falls
> all the way through to `None` instead.

Good:

> The tool already knows that a variable at the very end of a path is usually fine — e.g.
> `.../{$options['lang']}` still resolves, because nothing comes after it. But it only recognised
> that when the variable was the *entire* last piece of the path. Here the variable is glued to
> `.php` (`{$file}.php`), so the tool didn't recognise the same situation and gave up instead of
> resolving it. (Fixed in `resolver.rs`, in the function that decides this.)

This applies to explaining bugs, describing what a change does, and answering "why" questions —
anywhere the alternative would be a wall of function/variable names presented as if they were
self-explanatory.

## Hold code to a professional bar, in every circumstance — not only when something complains

"It compiles, the tests pass, the linter is quiet" is not the bar. Bring real engineering judgement
— about structure, naming, edge cases, what's tested, what's simplified vs. genuinely handled,
whether an abstraction is earned — to everything written for this project, by default, whether or
not anything is currently forcing the question. A lint warning, a compile error, an awkward
signature: these are just occasions where a shortcut happened to become visible. They are not the
boundary of where the standard applies. Absence of a visible complaint is not permission to stop
applying it.

One instance of this, so the shape of the failure is concrete rather than abstract: a lint fired
because a function's parameter count grew past a threshold. The lazy fix was to bundle the extra
parameters into a struct that made the number go down — compiled, tests passed, warning gone, and
still the wrong move, because the grouping was invented rather than real. The honest fix came from
tracing where each parameter actually originated: some were fixed for the whole scan, others
recomputed per file — a split `scan.rs` already made in how it computed them — so two structs
(`ScanContext`, `FileContext`) matching that real distinction were the right shape, not a bag sized
to satisfy a tool.

That's one example of the failure, not its definition. The same shortcut — reaching for whatever
makes a visible problem go away instead of the change that's actually correct — can show up
anywhere: a bug fix, a new feature, a test, an API design, a "simple" one-line change. Hold the same
standard everywhere, and when a shortcut's acceptability is genuinely unclear, say so and ask,
rather than shipping it quietly.

## Take the user's assertions at face value — don't go looking for evidence against them

When the user states something as fact — about this project, its history, why something is the way
it is, their own intent — treat it as true and act on it. This isn't only about running a command to
check: building a paragraph of reasoning that arrives at "yes, you're right" is the same move, just
done in prose instead of a shell command. Either way, don't report it back as if it "confirmed" or
"matched" what they said. That is not the same thing as asking a clarifying question when something
is genuinely ambiguous, and it's not the same as checking the current state of code or files before
acting on a plan — ordinary engineering diligence about *what the code currently does* is fine. This
is specifically about not treating a direct statement from the user as a claim requiring independent
verification (by tool or by reasoning), as though they might be mistaken or lying. If they themselves
signal uncertainty ("I think", "I'm not sure", "probably") that's an invitation to check; a flat
statement is not.

For example: told "there's no deliberate line-width convention here — if it looks that way, it's
because you did that in a previous session," the wrong response was running `git log` to check
whether a `rustfmt.toml` had ever existed, then citing that as confirmation. Told next, on a
different question, "the resolver should not be checking paths exist," the wrong response — with no
command run at all — was a paragraph explaining why that was correct followed by "that matches
exactly what you originally proposed." The right response in both cases was the same: just accept it
and act on it, agreement-sized, no derivation.
