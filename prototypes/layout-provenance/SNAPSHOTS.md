# Snapshots

Generated with `cargo run --manifest-path prototypes/layout-provenance/Cargo.toml -- --snapshot`. Colour is lost in the dump; run the prototype to judge colour.

### A mid-flight, 140x24

```
 repon 37 entities · list · 260ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│name                         branch                   sync      dirty  state                                                              │
│acquiring-gateway            main                     ≡         ·                                                                         │
│  └ fix/settlement-retry     fix/settlement-retry     ⠹         ⠹      ⠹                                                                  │
│  └ chore/bump-tonic         chore/bump-tonic         ≡         ·      merged                                                             │
│  └ spike/idempotency        spike/idempotency        ⠹         ⠹      ⠹                                                                  │
│vendor/legacy-terminal-sdk   ⠹                        ⠹         ⠹                                                                         │
│vendor/broken-checkout       ✗                        ✗         ✗                                                                         │
│scratch/perf-notes           main                     -         ●2                                                                        │
│  ∙ acquiring-gateway/protos v3                       ⠹         ⠹                                                                         │
│checkout-web                 main                     ⠹         ⠹                                                                         │
│checkout-web-e2e             main                     ⠹         ⠹                                                                         │
│ledger-core                  main                     ⠹         ⠹                                                                         │
│ledger-projections           main                     ⠹         ⠹                                                                         │
│merchant-portal              develop                  ⠹         ⠹                                                                         │
│merchant-portal-design       main                     ≡         ·                                                                         │
│payouts-scheduler            main                     ≡         ·                                                                         │
│payouts-rules                main                     ⠹         ⠹                                                                         │
│risk-scoring                 main                     ⠹         ⠹                                                                         │
│risk-features                main                     ⠹         ⠹                                                                         │
│terminal-firmware            ⠹                        ⠹         ⠹                                                                         │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### A settled, 140x24

```
 repon 37 entities · list · 12000ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│name                         branch                   sync      dirty  state                                                              │
│acquiring-gateway            main                     ≡         ·                                                                         │
│  └ fix/settlement-retry     fix/settlement-retry     ↑3        ●4     active                                                             │
│  └ chore/bump-tonic         chore/bump-tonic         ≡         ·      merged                                                             │
│  └ spike/idempotency        spike/idempotency        ≡         ●11    local only                                                         │
│vendor/legacy-terminal-sdk   master                   ?         ?                                                                         │
│vendor/broken-checkout       ✗                        ✗         ✗                                                                         │
│scratch/perf-notes           main                     -         ●2                                                                        │
│  ∙ acquiring-gateway/protos v3                       ↓12       ·                                                                         │
│checkout-web                 main                     ↓2        ·                                                                         │
│checkout-web-e2e             main                     ≡         ●1                                                                        │
│ledger-core                  main                     ↑1        ·                                                                         │
│ledger-projections           main                     ≡         ·                                                                         │
│merchant-portal              develop                  ↓41       ●7                                                                        │
│merchant-portal-design       main                     ≡         ·                                                                         │
│payouts-scheduler            main                     ≡         ·                                                                         │
│payouts-rules                main                     ↑2 ↓2     ·                                                                         │
│risk-scoring                 main                     ↓5        ●3                                                                        │
│risk-features                main                     ≡         ·                                                                         │
│terminal-firmware            trunk                    ≡         ·                                                                         │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### B mid-flight, 140x24

```
 repon 37 entities · list · 260ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│  name                         branch                   sync      dirty  state                                                            │
│  acquiring-gateway            main                     ≡         ·                                                                       │
│⠹   └ fix/settlement-retry     fix/settlement-retry                                                                                       │
│    └ chore/bump-tonic         chore/bump-tonic         ≡         ·      merged                                                           │
│⠹   └ spike/idempotency        spike/idempotency                                                                                          │
│⠹ vendor/legacy-terminal-sdk                                                                                                              │
│! vendor/broken-checkout                                                                                                                  │
│  scratch/perf-notes           main                     -         ●2                                                                      │
│⠹   ∙ acquiring-gateway/protos v3                                                                                                         │
│⠹ checkout-web                 main                                                                                                       │
│⠹ checkout-web-e2e             main                                                                                                       │
│⠹ ledger-core                  main                                                                                                       │
│⠹ ledger-projections           main                                                                                                       │
│⠹ merchant-portal              develop                                                                                                    │
│  merchant-portal-design       main                     ≡         ·                                                                       │
│  payouts-scheduler            main                     ≡         ·                                                                       │
│⠹ payouts-rules                main                                                                                                       │
│⠹ risk-scoring                 main                                                                                                       │
│⠹ risk-features                main                                                                                                       │
│⠹ terminal-firmware                                                                                                                       │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### B settled, 140x24

```
 repon 37 entities · list · 12000ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│  name                         branch                   sync      dirty  state                                                            │
│  acquiring-gateway            main                     ≡         ·                                                                       │
│    └ fix/settlement-retry     fix/settlement-retry     ↑3        ●4     active                                                           │
│    └ chore/bump-tonic         chore/bump-tonic         ≡         ·      merged                                                           │
│    └ spike/idempotency        spike/idempotency        ≡         ●11    local only                                                       │
│? vendor/legacy-terminal-sdk   master                                                                                                     │
│! vendor/broken-checkout                                                                                                                  │
│  scratch/perf-notes           main                     -         ●2                                                                      │
│    ∙ acquiring-gateway/protos v3                       ↓12       ·                                                                       │
│  checkout-web                 main                     ↓2        ·                                                                       │
│  checkout-web-e2e             main                     ≡         ●1                                                                      │
│  ledger-core                  main                     ↑1        ·                                                                       │
│  ledger-projections           main                     ≡         ·                                                                       │
│  merchant-portal              develop                  ↓41       ●7                                                                      │
│  merchant-portal-design       main                     ≡         ·                                                                       │
│  payouts-scheduler            main                     ≡         ·                                                                       │
│  payouts-rules                main                     ↑2 ↓2     ·                                                                       │
│  risk-scoring                 main                     ↓5        ●3                                                                      │
│  risk-features                main                     ≡         ·                                                                       │
│  terminal-firmware            trunk                    ≡         ·                                                                       │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### C mid-flight, 140x24

```
 repon 37 entities · list · 260ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│name                         branch                   sync      dirty  state      as of                                                   │
│acquiring-gateway            main                     ≡         ·                 now                                                     │
│  └ fix/settlement-retry     fix/settlement-retry                                 reading…                                                │
│  └ chore/bump-tonic         chore/bump-tonic         ≡         ·      merged     now                                                     │
│  └ spike/idempotency        spike/idempotency                                    reading…                                                │
│vendor/legacy-terminal-sdk                                                        reading…                                                │
│vendor/broken-checkout                                                            failed                                                  │
│scratch/perf-notes           main                     -         ●2                now                                                     │
│  ∙ acquiring-gateway/protos v3                                                   reading…                                                │
│checkout-web                 main                                                 reading…                                                │
│checkout-web-e2e             main                                                 reading…                                                │
│ledger-core                  main                                                 reading…                                                │
│ledger-projections           main                                                 reading…                                                │
│merchant-portal              develop                                              reading…                                                │
│merchant-portal-design       main                     ≡         ·                 now                                                     │
│payouts-scheduler            main                     ≡         ·                 now                                                     │
│payouts-rules                main                                                 reading…                                                │
│risk-scoring                 main                                                 reading…                                                │
│risk-features                main                                                 reading…                                                │
│terminal-firmware                                                                 reading…                                                │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### C settled, 140x24

```
 repon 37 entities · list · 12000ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│name                         branch                   sync      dirty  state      as of                                                   │
│acquiring-gateway            main                     ≡         ·                 11s                                                     │
│  └ fix/settlement-retry     fix/settlement-retry     ↑3        ●4     active     9s                                                      │
│  └ chore/bump-tonic         chore/bump-tonic         ≡         ·      merged     11s                                                     │
│  └ spike/idempotency        spike/idempotency        ≡         ●11    local only 11s                                                     │
│vendor/legacy-terminal-sdk   master                                               unknown                                                 │
│vendor/broken-checkout                                                            failed                                                  │
│scratch/perf-notes           main                     -         ●2                11s                                                     │
│  ∙ acquiring-gateway/protos v3                       ↓12       ·                 11s                                                     │
│checkout-web                 main                     ↓2        ·                 11s                                                     │
│checkout-web-e2e             main                     ≡         ●1                11s                                                     │
│ledger-core                  main                     ↑1        ·                 11s                                                     │
│ledger-projections           main                     ≡         ·                 11s                                                     │
│merchant-portal              develop                  ↓41       ●7                11s                                                     │
│merchant-portal-design       main                     ≡         ·                 11s                                                     │
│payouts-scheduler            main                     ≡         ·                 11s                                                     │
│payouts-rules                main                     ↑2 ↓2     ·                 11s                                                     │
│risk-scoring                 main                     ↓5        ●3                10s                                                     │
│risk-features                main                     ≡         ·                 11s                                                     │
│terminal-firmware            trunk                    ≡         ·                 9s                                                      │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### A detail beside list, 140x24

```
 repon 37 entities · detail (beside list) · 12000ms
╭ repos ─────────────────────────╮╭ detail (esc closes) ───────────────────────────────────────────────────────────────────────────────────╮
│  acquiring-gateway             ││fix/settlement-retry   worktree                                                                         │
│   └ fix/settlement-retry       ││~/dev/acquiring-gateway/fix/settlement-retry                                                            │
│   └ chore/bump-tonic           ││                                                                                                        │
│   └ spike/idempotency          ││branch    fix/settlement-retry   fresh 11s ago                                                          │
│  vendor/legacy-terminal-sdk    ││sync      3 ahead, 0 behind   fresh 9s ago                                                              │
│! vendor/broken-checkout        ││dirty     4 changed   fresh 9s ago                                                                      │
│  scratch/perf-notes            ││state     active   fresh 9s ago                                                                         │
│   ∙ acquiring-gateway/protos   ││                                                                                                        │
│  checkout-web                  ││recent                                                                                                  │
│  checkout-web-e2e              ││  9ab7712  Split the checkout reducer per step                                                          │
│  ledger-core                   ││  2c40f8e  Stop double-firing the analytics event                                                       │
│  ledger-projections            ││                                                                                                        │
│  merchant-portal               ││last action   fetch --all   (12 of 31 selected)                                                         │
│  merchant-portal-design        ││  step 1  ok      fetch origin, 3 refs updated                                                          │
│  payouts-scheduler             ││  step 2  skipped no upstream configured                                                                │
│  payouts-rules                 ││                                                                                                        │
│  risk-scoring                  ││                                                                                                        │
│  risk-features                 ││                                                                                                        │
│  terminal-firmware             ││                                                                                                        │
│  terminal-provisioning         ││                                                                                                        │
╰────────────────────────────────╯╰────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### A detail full frame, 88x24

```
 repon 37 entities · detail (full frame) · 12000ms
╭ detail (esc closes) ─────────────────────────────────────────────────────────────────╮
│fix/settlement-retry   worktree                                                       │
│~/dev/acquiring-gateway/fix/settlement-retry                                          │
│                                                                                      │
│branch    fix/settlement-retry   fresh 11s ago                                        │
│sync      3 ahead, 0 behind   fresh 9s ago                                            │
│dirty     4 changed   fresh 9s ago                                                    │
│state     active   fresh 9s ago                                                       │
│                                                                                      │
│recent                                                                                │
│  9ab7712  Split the checkout reducer per step                                        │
│  2c40f8e  Stop double-firing the analytics event                                     │
│                                                                                      │
│last action   fetch --all   (12 of 31 selected)                                       │
│  step 1  ok      fetch origin, 3 refs updated                                        │
│  step 2  skipped no upstream configured                                              │
│                                                                                      │
│                                                                                      │
│                                                                                      │
│                                                                                      │
│                                                                                      │
╰──────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A  B  C  → ▶   j/k  enter  esc  r  s  q
```

### A list only, 88x24

```
 repon 37 entities · list · 12000ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────╮
│name                         branch                   sync      dirty  state          │
│acquiring-gateway            main                     ≡         ·                     │
│  └ fix/settlement-retry     fix/settlement-retry     ↑3        ●4     active         │
│  └ chore/bump-tonic         chore/bump-tonic         ≡         ·      merged         │
│  └ spike/idempotency        spike/idempotency        ≡         ●11    local only     │
│vendor/legacy-terminal-sdk   master                   ?         ?                     │
│vendor/broken-checkout       ✗                        ✗         ✗                     │
│scratch/perf-notes           main                     -         ●2                    │
│  ∙ acquiring-gateway/protos v3                       ↓12       ·                     │
│checkout-web                 main                     ↓2        ·                     │
│checkout-web-e2e             main                     ≡         ●1                    │
│ledger-core                  main                     ↑1        ·                     │
│ledger-projections           main                     ≡         ·                     │
│merchant-portal              develop                  ↓41       ●7                    │
│merchant-portal-design       main                     ≡         ·                     │
│payouts-scheduler            main                     ≡         ·                     │
│payouts-rules                main                     ↑2 ↓2     ·                     │
│risk-scoring                 main                     ↓5        ●3                    │
│risk-features                main                     ≡         ·                     │
│terminal-firmware            trunk                    ≡         ·                     │
╰──────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A  B  C  → ▶   j/k  enter  esc  r  s  q
```

### B first frame, 140x24

```
 repon 37 entities · list · 40ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│  name                         branch                   sync      dirty  state                                                            │
│⠋ acquiring-gateway            main                                                                                                       │
│⠋   └ fix/settlement-retry                                                                                                                │
│⠋   └ chore/bump-tonic         chore/bump-tonic                                                                                           │
│⠋   └ spike/idempotency        spike/idempotency                                                                                          │
│⠋ vendor/legacy-terminal-sdk                                                                                                              │
│! vendor/broken-checkout                                                                                                                  │
│⠋ scratch/perf-notes           main                                                                                                       │
│⠋   ∙ acquiring-gateway/protos v3                                                                                                         │
│⠋ checkout-web                                                                                                                            │
│⠋ checkout-web-e2e                                                                                                                        │
│⠋ ledger-core                                                                                                                             │
│⠋ ledger-projections                                                                                                                      │
│⠋ merchant-portal                                                                                                                         │
│⠋ merchant-portal-design                                                                                                                  │
│⠋ payouts-scheduler                                                                                                                       │
│⠋ payouts-rules                                                                                                                           │
│⠋ risk-scoring                                                                                                                            │
│⠋ risk-features                                                                                                                           │
│⠋ terminal-firmware                                                                                                                       │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### B detail beside list, 140x24

```
 repon 37 entities · detail (beside list) · 12000ms
╭ repos ─────────────────────────╮╭ detail (esc closes) ───────────────────────────────────────────────────────────────────────────────────╮
│  acquiring-gateway             ││fix/settlement-retry   worktree                                                                         │
│   └ fix/settlement-retry       ││~/dev/acquiring-gateway/fix/settlement-retry                                                            │
│   └ chore/bump-tonic           ││                                                                                                        │
│   └ spike/idempotency          ││branch    fix/settlement-retry   fresh 11s ago                                                          │
│  vendor/legacy-terminal-sdk    ││sync      3 ahead, 0 behind   fresh 9s ago                                                              │
│! vendor/broken-checkout        ││dirty     4 changed   fresh 9s ago                                                                      │
│  scratch/perf-notes            ││state     active   fresh 9s ago                                                                         │
│   ∙ acquiring-gateway/protos   ││                                                                                                        │
│  checkout-web                  ││recent                                                                                                  │
│  checkout-web-e2e              ││  9ab7712  Split the checkout reducer per step                                                          │
│  ledger-core                   ││  2c40f8e  Stop double-firing the analytics event                                                       │
│  ledger-projections            ││                                                                                                        │
│  merchant-portal               ││last action   fetch --all   (12 of 31 selected)                                                         │
│  merchant-portal-design        ││  step 1  ok      fetch origin, 3 refs updated                                                          │
│  payouts-scheduler             ││  step 2  skipped no upstream configured                                                                │
│  payouts-rules                 ││                                                                                                        │
│  risk-scoring                  ││                                                                                                        │
│  risk-features                 ││                                                                                                        │
│  terminal-firmware             ││                                                                                                        │
│  terminal-provisioning         ││                                                                                                        │
╰────────────────────────────────╯╰────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A glyph in the cell  B row gutter, blank cells  C trailing age column  → ▶   j/k move  enter open  esc close  r refresh  s age  q quit
```

### B detail full frame, 88x24

```
 repon 37 entities · detail (full frame) · 12000ms
╭ detail (esc closes) ─────────────────────────────────────────────────────────────────╮
│fix/settlement-retry   worktree                                                       │
│~/dev/acquiring-gateway/fix/settlement-retry                                          │
│                                                                                      │
│branch    fix/settlement-retry   fresh 11s ago                                        │
│sync      3 ahead, 0 behind   fresh 9s ago                                            │
│dirty     4 changed   fresh 9s ago                                                    │
│state     active   fresh 9s ago                                                       │
│                                                                                      │
│recent                                                                                │
│  9ab7712  Split the checkout reducer per step                                        │
│  2c40f8e  Stop double-firing the analytics event                                     │
│                                                                                      │
│last action   fetch --all   (12 of 31 selected)                                       │
│  step 1  ok      fetch origin, 3 refs updated                                        │
│  step 2  skipped no upstream configured                                              │
│                                                                                      │
│                                                                                      │
│                                                                                      │
│                                                                                      │
│                                                                                      │
╰──────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A  B  C  → ▶   j/k  enter  esc  r  s  q
```

### B list only, 88x24

```
 repon 37 entities · list · 12000ms
╭ repos (enter opens detail) ──────────────────────────────────────────────────────────╮
│  name                         branch                   sync      dirty  state        │
│  acquiring-gateway            main                     ≡         ·                   │
│    └ fix/settlement-retry     fix/settlement-retry     ↑3        ●4     active       │
│    └ chore/bump-tonic         chore/bump-tonic         ≡         ·      merged       │
│    └ spike/idempotency        spike/idempotency        ≡         ●11    local only   │
│? vendor/legacy-terminal-sdk   master                                                 │
│! vendor/broken-checkout                                                              │
│  scratch/perf-notes           main                     -         ●2                  │
│    ∙ acquiring-gateway/protos v3                       ↓12       ·                   │
│  checkout-web                 main                     ↓2        ·                   │
│  checkout-web-e2e             main                     ≡         ●1                  │
│  ledger-core                  main                     ↑1        ·                   │
│  ledger-projections           main                     ≡         ·                   │
│  merchant-portal              develop                  ↓41       ●7                  │
│  merchant-portal-design       main                     ≡         ·                   │
│  payouts-scheduler            main                     ≡         ·                   │
│  payouts-rules                main                     ↑2 ↓2     ·                   │
│  risk-scoring                 main                     ↓5        ●3                  │
│  risk-features                main                     ≡         ·                   │
│  terminal-firmware            trunk                    ≡         ·                   │
╰──────────────────────────────────────────────────────────────────────────────────────╯
 ◀ ←  A  B  C  → ▶   j/k  enter  esc  r  s  q
```

