# Running Loop from scripts (`--print`)

`loop --print "<prompt>"` runs one turn without the terminal UI and exits.

```bash
loop --print "Summarise the README"                              # the answer as text
loop --print "Summarise the README" --output-format json         # one result object
loop --print "Fix the failing test" --output-format stream-json  # every update, then the result
```

## Output formats

- **`text`** (default): the text of the agent's last message.
- **`json`**: one object when the turn ends:
  ```json
  {"type":"result","session":"…","stop":"end_turn","text":"…","usage":{"input_tokens":7531,"output_tokens":21,"total_tokens":7552},"error":null,"exit_code":0}
  ```
  `stop` is `end_turn`, `max_tokens`, `max_turn_requests`, `refusal`, `cancelled` or `error`. When the turn didn't complete, `error` says why.
- **`stream-json`**: one JSON line per update of the turn, as it happens, then the same `result` object as the last line.
  - Each update line is a `loop-protocol` `UpdateEnvelope`: `{"v":1,"session":…,"turn":…,"seq":…,"origin":"live","update":{"type":"message_chunk",…}}`.
  - Progress notices such as "preparing turn…" can come before `turn_started`.
  - Tell the last line apart by its `"type":"result"`.

stdout carries only the selected format. Notices, warnings and the trace id go to stderr.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | The turn completed. |
| 1 | The turn failed, or Loop failed to start, for example with an unknown model. A startup failure prints nothing on stdout. A turn also fails when its answer is incomplete (see below). |
| 2 | Usage error, such as an unknown flag or value. |
| 3 | Contact with the agent was lost mid-turn. Edits or commands may already have run, so don't simply retry. |
| 130 | The turn was cancelled. |

### Incomplete answers

- **Output limit.** When a response stops at the model's output limit (`max_tokens`) with no tool call, Loop asks the model to continue, up to 3 times per prompt, and prints the joined answer. If the turn still ends at the limit, `stop` is `max_tokens` and the exit code is 1; in `text` format the partial answer goes to stdout, then one `loop: …` line to stderr.
- **No answer.** A turn that ran no tool and ends with an empty message exits 1 with `loop: the agent ended the turn with no text`. A turn that ran tools may end with an empty message and still exits 0.

In `json` and `stream-json` the reason is in the result's `error` field.

## Permissions

Nobody can approve anything in a `--print` run, so a tool call that needs approval is refused, and the model is told why. See [Permissions](permissions.md#full-access) for `--permission-mode` and [Rules](permissions.md#rules) for allow rules.
