Tasks: 36 (35 ran, 1 skipped as too slow, 0 harness errors)

| Check | Passed |
| --- | --- |
| Fix under foil: every test outcome = baseline | 35 / 35 |
| After commit, outside foil: every test outcome = baseline | 35 / 35 |
| Without the fix under foil: the target tests fail | 35 / 35 |
| After abort: workspace unchanged | 35 / 35 |
| While the branch exists: workspace unchanged | 35 / 35 |
| foil diff lists every file the patches touch | 35 / 35 |
| foil commit succeeds | 35 / 35 |

Baseline resolved (all target and regression tests pass, no foil): 31 / 35; unresolved ones fail identically with and without foil (environment differences from SWE-bench's Docker images).

| Task | Version | Baseline s | Under foil s | Resolved | All checks |
| --- | --- | --- | --- | --- | --- |
| django__django-10924 | 3.0 | 0.9 | 1.0 | yes | ok |
| django__django-10914 | 3.0 | 1.1 | 1.3 | no | ok |
| django__django-11001 | 3.0 | 1.5 | 1.8 | yes | ok |
| django__django-11099 | 3.0 | 1.4 | 1.6 | yes | ok |
| django__django-11797 | 3.1 | 1.4 | 1.3 | yes | ok |
| django__django-11564 | 3.1 | 2.9 | 2.9 | yes | ok |
| django__django-11815 | 3.1 | 0.9 | 0.9 | yes | ok |
| django__django-12856 | 3.2 | 1.2 | 1.3 | yes | ok |
| django__django-12908 | 3.2 | 1.6 | 2.1 | yes | ok |
| django__django-13447 | 4.0 | 2.2 | 1.7 | yes | ok |
| django__django-12915 | 3.2 | 3.2 | 3.0 | no | ok |
| django__django-13925 | 4.0 | 1.1 | 1.2 | yes | ok |
| django__django-14787 | 4.1 | 0.9 | 1.0 | yes | ok |
| django__django-13710 | 4.0 | 5.1 | 5.7 | yes | ok |
| django__django-14915 | 4.1 | 1.4 | 1.4 | yes | ok |
| django__django-15738 | 4.2 | 1.6 | 1.5 | yes | ok |
| django__django-14997 | 4.1 | 4.1 | 4.1 | yes | ok |
| django__django-15781 | 4.2 | 2.0 | 1.9 | no | ok |
| django__django-15789 | 4.2 | 1.2 | 1.3 | no | ok |
| django__django-16408 | 5.0 | 1.2 | 1.2 | yes | ok |
| django__django-16595 | 5.0 | 1.0 | 1.0 | yes | ok |
| django__django-16527 | 5.0 | 2.8 | 3.3 | yes | ok |
| sympy__sympy-11400 | 1.0 | 2.3 | 2.3 | yes | ok |
| sympy__sympy-13480 | 1.1 | 14.6 | 13.4 | yes | ok |
| sympy__sympy-15011 | 1.2 | 5.4 | 5.3 | yes | ok |
| sympy__sympy-15308 | 1.4 | 5.9 | 6.1 | yes | ok |
| sympy__sympy-16503 | 1.5 | 12.2 | 12.2 | yes | ok |
| sympy__sympy-19254 | 1.7 | 3.1 | 3.5 | yes | ok |
| sympy__sympy-20322 | 1.8 | 23.7 | 23.0 | yes | ok |
| sympy__sympy-18057 | 1.6 | 249.5 | 216.4 | yes | ok |
| sympy__sympy-21379 | 1.9 | 35.0 | 33.6 | yes | ok |
| sympy__sympy-22714 | 1.10 | 9.2 | 9.1 | yes | ok |
| sympy__sympy-23117 | 1.11 | 3.6 | 3.9 | yes | ok |
| sympy__sympy-24066 | 1.12 | 4.0 | 4.4 | yes | ok |
| sympy__sympy-24909 | 1.13 | 2.3 | 2.1 | yes | ok |
| sympy__sympy-11870 | 1.1 | — | — | — | skipped: baseline tests took over 900 s on this VM |

Median test-run time under foil / baseline: 1.00 (the foil run also applies the patches; baseline and foil runs are not interleaved)
