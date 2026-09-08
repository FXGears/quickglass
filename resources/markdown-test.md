# Markdown Conformance Test

A systematic exercise of CommonMark plus every extension `pulldown-cmark` enables
under `Options::all()`. Each section is labelled with what it tests and what
correct output looks like, so a wrong render is obvious without a reference open.

Open the same file in both renderers to compare:

```
quickglass.exe resources\markdown-test.md
quickglass.exe resources\markdown-test.md --beta_render
```

---

## 1. Headings

# H1 via ATX
## H2 via ATX
### H3 via ATX
#### H4 via ATX
##### H5 via ATX
###### H6 via ATX

H1 via Setext
=============

H2 via Setext
-------------

### Closed ATX form ###

*Expected: six distinct sizes. H1 and H2 carry a bottom rule. H4 through H6 are
body-sized but bold. The closing hashes are not part of the text.*

---

## 2. Paragraphs and line breaks

This is one paragraph. It contains a soft break at the end of this line
which should render as a single space, not a new line.

This line ends with two spaces to force a hard break.  
So this text belongs to the same paragraph but starts on a new line.

This line ends with a backslash to force a hard break.\
So does this one.

*Expected: three paragraphs. The first is a single flowing line. The second and
third each break internally without extra vertical gap.*

---

## 3. Emphasis

*Italic with asterisks* and _italic with underscores_.

**Bold with asterisks** and __bold with underscores__.

***Bold italic*** and ___bold italic___ and **_mixed nesting_**.

~~Strikethrough~~ and ~~**bold strikethrough**~~.

Intraword emphasis: un*frigging*believable. Underscores should not apply
intraword: snake_case_identifier stays literal.

Nested: **bold containing *italic* inside** and *italic containing **bold** inside*.

*Expected: italic slants, bold is heavier and lighter in colour, strikethrough
has a line through it. `snake_case_identifier` shows literal underscores.*

---

## 4. Inline code

Simple `inline code`. Code with `*asterisks*` and `_underscores_` that must stay
literal. Code containing a backtick: `` a ` b ``. Code with double backticks
inside: ``` x `` y ```.

Code with markup-looking content: `<div class="x">`, `&amp;`, `[link](url)`.

Long inline code that should wrap with the surrounding text rather than overflow:
`this_is_a_very_long_identifier_that_keeps_going_and_going_to_test_wrapping_behaviour`.

*Expected: monospace, distinct background pill, and no interpretation of the
contents.*

---

## 5. Links

Inline link to [example.com](https://example.com).

Inline link [with a title](https://example.com "Hover title here").

Autolink: <https://example.com/autolink>

Bare URL for GFM autolinking: https://example.com/bare

Email autolink: <someone@example.com>

Reference link [full reference][ref-one] and [collapsed reference][] and
[shortcut reference].

Relative link to a [local file](./HANDOFF.md).

Anchor link to [section 12](#12-tables).

Link containing formatting: [**bold link text**](https://example.com).

[ref-one]: https://example.com/full-reference
[collapsed reference]: https://example.com/collapsed
[shortcut reference]: https://example.com/shortcut

*Expected: every one renders as link-coloured text. Reference definitions
themselves produce no visible output.*

---

## 6. Images

Inline image: ![QuickGlass icon](icon-64.png)

Image with title: ![QuickGlass icon](icon-64.png "The icon")

Reference image: ![reference form][img-ref]

Broken image path: ![missing alt text](does-not-exist.png)

Image inside a link: [![linked image](icon-64.png)](https://example.com)

[img-ref]: icon-64.png

*Expected: the WebView2 path cannot resolve relative paths because the document
is loaded as inline HTML, so all four fail there. The native path shows
`[image: alt]` placeholders by design.*

---

## 7. Blockquotes

> A simple blockquote.

> A blockquote spanning
> two source lines that should join into one paragraph.

> Lazy continuation: the second line
omits its marker but still belongs to the quote.

> First paragraph of a quote.
>
> Second paragraph of the same quote.

> ## Heading inside a quote
>
> - list inside a quote
> - second item
>
> ```
> code block inside a quote
> ```
>
> Final paragraph.

> Level one
> > Level two
> > > Level three

*Expected: a left bar per nesting level, muted text, and correct indentation.
Nested quotes show stacked bars.*

---

## 8. Unordered lists

- Item with a hyphen marker
- Second item

* Item with an asterisk marker
* Second item

+ Item with a plus marker
+ Second item

Nested:

- Level one
  - Level two
    - Level three
      - Level four
- Back to level one

Tight versus loose:

- Tight item one
- Tight item two

- Loose item one, separated by blank lines

- Loose item two

Item containing multiple blocks:

- First paragraph of the item.

  Second paragraph of the same item.

  ```
  code block inside a list item
  ```

  > quote inside a list item

- Next item.

Item with a long wrapping line to confirm the hanging indent aligns past the
marker:

- This item deliberately contains enough text that it must wrap onto at least a
  second and probably a third line so the alignment of continuation lines is
  clearly visible against the bullet.

*Expected: bullets aligned, continuation lines indented past the marker, nesting
indented one level each time.*

---

## 9. Ordered lists

1. First
2. Second
3. Third

Starting at a number other than one:

5. Five
6. Six
7. Seven

All ones, which should still count up:

1. Renders as 1
1. Renders as 2
1. Renders as 3

Parenthesis delimiter:

1) Alternate delimiter
2) Second

Nested mixed:

1. Ordered level one
   - Unordered level two
     1. Ordered level three
2. Back to ordered level one

*Expected: numbers increment correctly, including the list starting at five.*

---

## 10. Task lists

- [ ] Unchecked task
- [x] Checked task, lowercase
- [X] Checked task, uppercase
- [ ] Task with **bold** and `code`
  - [ ] Nested unchecked
  - [x] Nested checked

*Expected: checkboxes. The native renderer does not implement task list markers
yet, so these should surface as a visible gap.*

---

## 11. Code blocks

Fenced with a language:

```rust
fn main() {
    let greeting = "hello";
    println!("{greeting}, world");
}
```

Fenced with no language:

```
plain preformatted text
    with preserved   spacing
```

Tilde fenced:

~~~python
def main():
    print("tilde fence")
~~~

Fence containing backticks:

````
```
nested fence stays literal
```
````

Indented code block, four spaces:

    indented code line one
    indented code line two

Code block with a very long line that should not wrap and instead needs
horizontal handling:

```
this_single_line_is_intentionally_far_too_wide_to_fit_in_the_window_and_keeps_going_well_past_any_reasonable_column_limit_to_test_overflow_behaviour
```

Code block with markup-looking content:

```html
<div class="example">
  <p>&amp; entities &lt;stay&gt; literal</p>
</div>
```

*Expected: monospace on a panel with rounded corners. No wrapping inside code.
The CSS path scrolls horizontally; the native path currently wraps instead.*

---

## 12. Tables

Basic:

| Column A | Column B |
|---|---|
| a1 | b1 |
| a2 | b2 |

Alignment:

| Left | Centre | Right |
|:-----|:------:|------:|
| l1 | c1 | r1 |
| left two | centre two | right two |

Inline formatting in cells:

| Feature | Syntax | Note |
|---|---|---|
| Bold | `**x**` | **applied** |
| Italic | `*x*` | *applied* |
| Code | `` `x` `` | `applied` |
| Link | `[a](b)` | [applied](https://example.com) |
| Strike | `~~x~~` | ~~applied~~ |

Ragged rows, fewer and more cells than the header:

| One | Two | Three |
|---|---|---|
| only one |
| a | b | c | d |

Empty cells:

| A | B | C |
|---|---|---|
|  | b |  |

Wide table to test horizontal fit:

| Identifier | Description | Default | Range | Unit | Notes |
|---|---|---|---|---|---|
| `max_width` | Content column width | 860 | 320-1600 | px | Matches CSS |
| `line_height` | Body leading | 1.6 | 1.0-2.5 | ratio | Uniform |

*Expected: header shaded, grid lines, alignment respected. The native renderer
uses equal column widths by design, so column proportions will differ from the
CSS path.*

---

## 13. Footnotes

Text with a footnote reference[^1] and a second one[^named].

[^1]: The first footnote body.
[^named]: A named footnote with **formatting** and a second sentence.

*Expected: superscript markers and a footnote section. Neither renderer
implements footnotes yet, so this should be visibly missing.*

---

## 14. Definition lists

Term one
: Definition of term one.

Term two
: First definition of term two.
: Second definition of term two.

*Expected: indented definitions. Not implemented in the native renderer.*

---

## 15. Math

Inline math: $E = mc^2$ appears within this sentence.

Display math:

$$
\int_{0}^{\infty} e^{-x^2}\,dx = \frac{\sqrt{\pi}}{2}
$$

*Expected: neither renderer typesets math. Expect literal text or raw markers.*

---

## 16. HTML

Inline HTML: this word is <strong>strong via HTML</strong> and this is
<em>emphasised via HTML</em>.

Block HTML:

<div align="center">
  <p>Centred block via raw HTML</p>
</div>

<details>
<summary>Collapsed section</summary>

Hidden content inside a details element.

</details>

HTML comment, which should produce nothing visible:

<!-- this comment must not appear in output -->

*Expected: the CSS path honours HTML. The native path deliberately shows raw
markup as literal monospace text rather than interpreting it, since a viewer
should not execute markup.*

---

## 17. Escapes and entities

Backslash escapes: \*not italic\*, \_not italic\_, \`not code\`, \# not a
heading, \[not a link\], \\ literal backslash.

Character entities: &amp; &lt; &gt; &quot; &copy; &mdash; &hellip; &nbsp; &#65;
&#x42;

Literal characters that need care: * _ ` # [ ] ( ) { } + - . ! | < > &

*Expected: escaped characters appear literally. Entities resolve to & < > "
(c) em-dash ellipsis, non-breaking space, A, B.*

---

## 18. Smart punctuation

"Double quotes" and 'single quotes' should curl. An em-dash --- like this, an
en-dash -- like that, and an ellipsis... at the end.

*Expected: with smart punctuation enabled, quotes curl and dashes convert. If
they stay straight, the option is not reaching the parser.*

---

## 19. Superscript and subscript

Superscript: X^2^ and water is H~2~O.

*Expected: only renders if the superscript and subscript extensions are active.
Otherwise the carets and tildes appear literally, or the tildes may be
interpreted as strikethrough.*

---

## 20. Unicode and text shaping

Latin with diacritics: àéîõü ÀÉÎÕÜ ß æ ø å

Greek: αβγδε ΑΒΓΔΕ

Cyrillic: абвгде АБВГДЕ

CJK: 日本語のテキスト 中文文本 한국어 텍스트

Right to left: العربية والعبرية עברית

Emoji: 🚀 ✅ ⚠️ 📄 🎯 and a family sequence 👨‍👩‍👧‍👦 and a flag 🇺🇸

Combining marks: e◌́ a◌̈ n◌̃

Zero-width and wide punctuation: a​b 「引用」 （括弧）

Monospace fallback in code: `日本語 αβγ 🚀`

*Expected: DirectWrite should fall back across scripts without tofu boxes. RTL
runs should read right to left. Emoji may render monochrome depending on the
font fallback chain.*

---

## 21. Long-line and wrapping stress

A single paragraph with no short words to test greedy wrapping behaviour across
a narrow window: pneumonoultramicroscopicsilicovolcanoconiosis
hippopotomonstrosesquippedaliophobia thyroparathyroidectomized
dichlorodifluoromethane immunoelectrophoretically psychophysicotherapeutics.

An unbroken string with no break opportunities at all:

AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA

A URL long enough to need breaking:
https://example.com/very/deep/path/that/keeps/going/for/quite/a/while/until/it/exceeds/the/column/width/index.html

*Expected: normal text wraps at word boundaries. The unbroken string will
overflow or clip; note which.*

---

## 22. Structural edge cases

Empty blockquote:

>

Heading immediately followed by a list with no blank line:

### Heading then list
- Item directly after a heading

Consecutive rules:

---
***
___

Two tables with nothing between them:

| X |
|---|
| 1 |

| Y |
|---|
| 2 |

Heading with trailing whitespace and inline code: ### `code in heading`

### Heading with **bold**, *italic*, `code`, and [a link](https://example.com)

Deeply nested mixture:

1. Ordered
   - Unordered
     > Quote
     >
     > ```
     > code
     > ```
     - Deeper unordered
       1. Deeper ordered

*Expected: no crashes, no runaway indentation, rules render as three separate
lines.*

---

## 23. Final section

If this line is visible and the scrollbar thumb has reached the bottom of its
track, vertical layout and scroll extent are both correct.

End of test document.
