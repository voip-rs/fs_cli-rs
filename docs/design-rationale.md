# fs_cli design rationale

## Reporting what the switch will set, never judging it

The originate check reports the values a dial string will actually install on the channel; it does not tell the operator the string is wrong. It cannot: the switch consumes escapes in the dial string before installing anything, and every reading of a typed block yields the value after that consumption, not the value its author meant. The two are indistinguishable in the text. The check names the delivered values and leaves the comparison against intent to the operator, who is the only party holding it.

The report fires only for a dial string carrying characters whose delivered value depends on escaping depth. A string without them arrives as written, and a diagnostic on it is noise; a check that cries wolf is one the operator switches off, after which it protects nobody.

Comparing a canonical re-render of the parsed dial string against the text as typed is not a usable signal for this and must not be reintroduced as one. The renderer rebuilds from parsed pairs rather than echoing input, normalising quoting, escaping and repeated keys, so it differs from correct input as readily as from mangled input.

## Correcting a dial string refuses ambiguity instead of resolving it

The correcting mode rewrites variable blocks so the channel receives the values as typed, bounded by two rules.

A block whose text reads more than one way is refused and sent unchanged. Where an escape character appears in a block, whether it belongs to the value or protects the character after it is not decidable from the text, and guessing either way corrupts the call for half of the inputs. The report says why the line was left alone.

A rewrite is verified before it is sent: the rewritten string is read back through the switch's passes and must yield the values read out of the original. A correction that cannot prove it delivers is discarded and the line is sent as typed. Correction never blocks a command and never alters one silently.

Reading a quote in a block value as a literal character is a choice rather than a reading the text compels. It matches how these values arrive — names and civic addresses carrying apostrophes — and it is why the escape character above cannot be treated the same way.

## Operator diagnostics do not go through the log

Anything the operator must see reaches them through the printer, not the tracing subscriber. The subscriber's level follows the ESL debug setting, which is off by default, so a logged warning is invisible to exactly the operator who has not gone looking for one. In batch mode the split that matters for a pipe holds: results on stdout, diagnostics on stderr.

## Batch exit status follows whether a command was sent

The exit status of a batch run says whether anything may have reached the switch, not what kind of error ended it. A failure before the first command is handed to the library means nothing was applied; any failure after it means the outcome is unknown, because the library reports a lost connection the same way whether the command reached the wire or not. A disconnect failing after every command was answered never alters the status.

A refused command exits as success unless the caller opts in, as stock fs_cli does. Opted in, a synchronous refusal stops further commands, since later ones may depend on it, but jobs already submitted are still awaited and reported, since they may have been applied. A refused job does not stop the run: it arrives asynchronously, so stopping on it would make which commands are sent depend on timing.
