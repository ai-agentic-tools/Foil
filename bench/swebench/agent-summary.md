31 tasks with both arms; 0 harness errors

| | without foil | with foil (strict) |
| --- | --- | --- |
| Resolved | 17 / 31 | 19 / 31 |
| Median agent time | 178 s | 152 s |
| Median turns | 48 | 38 |
| Total cost | $9.97 | $10.54 |
| Stopped by the 60-turn limit or an error | 9 | 10 |

foil checks:
- workspace unchanged until commit: 31 / 31
- commit ok: 31 / 31
- API key in agent stderr: 0
- hosts the proxy blocked: none

same outcome in both arms: 27 / 31
resolved only without foil: django__django-13925
resolved only with foil: django__django-11815, django__django-14997, sympy__sympy-11400

| Task | without foil | with foil |
| --- | --- | --- |
| django__django-10924 | yes (200 s, 50 turns) | yes (151 s, 38 turns) |
| django__django-11001 | yes (138 s, 35 turns) | yes (92 s, 19 turns) |
| django__django-11099 | yes (21 s, 7 turns) | yes (28 s, 9 turns) |
| django__django-11564 | no (228 s, 61 turns) | no (223 s, 61 turns) |
| django__django-11797 | no (317 s, 61 turns) | no (284 s, 61 turns) |
| django__django-11815 | no (119 s, 30 turns) | yes (184 s, 59 turns) |
| django__django-12856 | yes (143 s, 50 turns) | yes (253 s, 48 turns) |
| django__django-12908 | no (212 s, 61 turns) | no (408 s, 61 turns) |
| django__django-13447 | yes (97 s, 15 turns) | yes (138 s, 32 turns) |
| django__django-13710 | yes (350 s, 54 turns) | yes (187 s, 44 turns) |
| django__django-13925 | yes (178 s, 61 turns) | no (236 s, 61 turns) |
| django__django-14787 | yes (66 s, 17 turns) | yes (68 s, 20 turns) |
| django__django-14915 | yes (62 s, 17 turns) | yes (84 s, 24 turns) |
| django__django-14997 | no (232 s, 59 turns) | yes (231 s, 61 turns) |
| django__django-15738 | no (303 s, 61 turns) | no (273 s, 61 turns) |
| django__django-16408 | no (583 s, 61 turns) | no (273 s, 61 turns) |
| django__django-16527 | yes (12 s, 4 turns) | yes (14 s, 4 turns) |
| django__django-16595 | yes (107 s, 33 turns) | yes (113 s, 23 turns) |
| sympy__sympy-11400 | no (95 s, 34 turns) | yes (169 s, 61 turns) |
| sympy__sympy-13480 | yes (16 s, 4 turns) | yes (21 s, 6 turns) |
| sympy__sympy-15011 | yes (181 s, 39 turns) | yes (148 s, 32 turns) |
| sympy__sympy-15308 | no (138 s, 48 turns) | no (134 s, 44 turns) |
| sympy__sympy-16503 | no (328 s, 61 turns) | no (457 s, 61 turns) |
| sympy__sympy-18057 | yes (178 s, 29 turns) | yes (101 s, 22 turns) |
| sympy__sympy-19254 | no (252 s, 61 turns) | no (204 s, 55 turns) |
| sympy__sympy-20322 | no (346 s, 61 turns) | no (325 s, 61 turns) |
| sympy__sympy-21379 | no (314 s, 60 turns) | no (152 s, 32 turns) |
| sympy__sympy-22714 | yes (128 s, 17 turns) | yes (115 s, 28 turns) |
| sympy__sympy-23117 | yes (72 s, 15 turns) | yes (67 s, 17 turns) |
| sympy__sympy-24066 | yes (279 s, 57 turns) | yes (215 s, 35 turns) |
| sympy__sympy-24909 | no (99 s, 21 turns) | no (107 s, 27 turns) |
