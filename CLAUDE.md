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
