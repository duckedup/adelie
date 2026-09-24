A store written by adelie 0.5.0 (manifest v1 before D0012's ids), kept byte-for-byte so a test
can prove an old manifest still opens and gets ids assigned. Never regenerate it with a newer
adelie: its whole value is that it was written by the old code.

- `main.events` (id, msg): segments 0 (`[3,"c"], [1,"a"]`) and 1 (`[2,"b"]`), under `main/events/_/`
- `main.users` (id, msg): segment 2 (`[7,"u"]`), under `main/users/_/`
