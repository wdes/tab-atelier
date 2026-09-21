# Support d'agents dans tab-atelier — état des lieux et généralisation

**Date** : 2026-09-18 · **Auteur** : Amaury (avec Claude Code)
**Branche** : `feat/codex-agent-support` · **Objet** : comprendre comment tab-atelier
suit et relance un agent, ce qui est spécifique à Claude, et ce qu'il faudrait
pour généraliser (cas pilote : codex).

---

## 1. Le mécanisme actuel, en une phrase

**L'agent déclare lui-même son état**, via des *hooks* qui appellent une petite
commande CLI. tab-atelier ne devine rien : il reçoit.

```
Claude Code ──hook──▶ tab-atelier set-status thinking|waiting|error|idle
                                    --kind claude --session <session_id>
                                    --label <texte court>
```

Le tab stocke alors : un **état** (pour la LED), un **libellé** (ce qui s'affiche),
et — c'est la clé — le **`agent_session_id`** et l'**`agent_kind`**.

## 2. Les quatre briques

| # | Brique | Où | Rôle |
|---|---|---|---|
| 1 | `TabLed` | `src/lib.rs` | État affiché : `Thinking`, `Waiting`, `Error`, **`Dead`** |
| 2 | `compute_tab_led()` | `src/lib.rs` | Calcule la LED depuis l'état déclaré + la vivacité du process |
| 3 | `POST /tabs/{id}/status` | `src/api/handlers/cards.rs` | Canal d'entrée (`thinking`/`waiting`/`error`/`idle` + `label`, `session_id`, `agent_kind`) |
| 4 | `build_agent_resume_command(kind, session_id)` | `src/lib.rs:689` | Construit la commande de reprise après reboot |

### Le canal d'entrée (3)

Déjà **générique** : il accepte `agent_kind` en paramètre, sans liste blanche.
Rien à changer pour un nouvel agent — c'est le point important.

### Le point de branchement (4)

```rust
match kind {
    "claude" => Some(format!("claude --resume {session_id}")),
    _ => None,          // ← codex tombe ici : aucune reprise
}
```

**C'est le seul endroit réellement spécifique à Claude.**

### La LED `Dead` — déjà prévue

`TabLed::Dead` est décrit dans le code comme : *« session ancrée mais le process
agent a disparu — à relancer »*. Autrement dit, **le cas « l'agent est mort, il
faut le relancer » est déjà modélisé et affiché**. Il manque seulement la
relance automatique.

## 3. Ce qui est spécifique à Claude

| Élément | Claude | Codex |
|---|---|---|
| Déclarer son état | hooks (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `Stop`, `PermissionRequest`, `StopFailure`, `SessionEnd`) | **aucun hook** |
| Fournir son `session_id` | dans le payload du hook (`jq -r .session_id`) | **à déduire** |
| Reprendre une session | `claude --resume <id>` | `codex resume <id>` ✅ existe |
| Détection du kind | déclaré par le hook | **à déduire** |

**Conclusion** : les deux manques pour codex sont *« qui suis-je »* (kind + session
id) et *« quel est mon état »*. Le reste de la mécanique (LED, stockage, reprise,
reboot) est déjà là.

## 4. Comment combler les deux manques pour codex

### 4.1 Retrouver le `session_id`

Codex écrit une session par fichier :

```
~/.codex/sessions/<AAAA>/<MM>/<JJ>/rollout-<horodatage>-<UUID>.jsonl
```

Vérifié : **l'UUID du nom de fichier EST le `session_id`**, et le fichier contient
un en-tête `session_meta` avec `session_id`, `cwd` et `timestamp`.

Deux stratégies :

- **A — `codex resume --last`** : simple, mais ambigu dès qu'il y a plusieurs
  sessions (ce qui est le cas ici). À écarter pour une reprise par tab.
- **B — retrouver le rollout par `cwd`** : le tab connaît son répertoire de
  travail ; on prend le rollout le plus récent dont `session_meta.cwd` correspond.
  Robuste, sans dépendre d'un état externe. **Retenue.**

### 4.2 Connaître l'état

Trois voies possibles :

| Voie | Avantage | Inconvénient |
|---|---|---|
| **Pattern d'écran** (ce que fait déjà le superviseur compta) | marche sans coopération de l'agent ; `• Working (`, `Ask Codex to do anything`, menu d'approbation sont fiables | fragile à une mise à jour d'interface ; dépend de la largeur du terminal |
| **Wrapper** : on lance codex via un script qui pousse l'état | fiable, explicite | ne couvre que les lancements via ce wrapper (pas `codex` tapé à la main) |
| **Lecture du rollout** : codex journalise chaque tour | fiable, passif | léger décalage (le fichier est écrit en fin de tour) |

**Recommandation** : *pattern d'écran* comme socle (déjà éprouvé sur le chantier
compta, où il tourne depuis plusieurs jours), avec le *rollout* comme confirmation.

## 5. Vers un support générique

L'architecture actuelle est **presque** générique — le canal d'état accepte déjà
n'importe quel `agent_kind`. Ce qui reste spécifique tient en un seul `match`.

**Proposition : une table de descripteurs d'agents**, plutôt qu'un `match` qui
grossit à chaque nouvel agent.

```
AgentDescriptor {
    kind:           "claude" | "codex" | …
    resume:         |session_id| -> "claude --resume {id}" | "codex resume {id}"
    find_session:   |cwd|        -> comment retrouver l'id (hook, rollout…)
    state_source:   Hook | ScreenPattern | Rollout
    screen_hints:   motifs « en cours » / « au repos » / « attend validation »
}
```

Bénéfices : ajouter un agent = ajouter une entrée, pas toucher la logique ;
les motifs d'écran deviennent des données (ajustables sans recompiler la logique) ;
on peut tester un nouvel agent sans risque de casser Claude.

**Limite à assumer** : un agent qui n'a *ni* hook *ni* journal lisible ne peut être
suivi que par pattern d'écran, donc approximativement. Il faut le dire, pas le masquer.

## 6. Ce qui n'est pas fait ici

- La table de descripteurs ci-dessus est une **proposition**, pas une implémentation.
- Le périmètre retenu pour la première PR est **codex uniquement** (voir le
  changelog de la branche) : un `match` de plus, pas une refonte.
- Les autres agents (gemini, aider…) n'ont pas été étudiés : ils nécessiteraient
  chacun la même paire de questions (« sait-il déclarer son état ? sait-il reprendre ? »).

## 6bis. Découvertes complémentaires (vérifiées le 2026-09-18)

### `session_index.jsonl` — un résumé lisible par session

Codex tient un index global, **bien plus pratique que les rollouts** :

```json
{"id":"01a0b392-…","thread_name":"Rappelle la facturation Terre","updated_at":"…"}
{"id":"01a0b4ff-…","thread_name":"Préparer factures janvier 2026","updated_at":"…"}
```

Le champ **`thread_name`** est un intitulé court, **écrit par l'agent lui-même**.
C'est exactement la matière d'un « résumé de session dans l'onglet » (demande 1) :
pas besoin d'analyser la conversation, l'information existe déjà.

**Limite** : l'index ne porte **pas le `cwd`** (contrairement aux rollouts). Pour
rattacher une entrée à un tab, il faut croiser les deux : `id` depuis l'index,
`cwd` depuis le `session_meta` du rollout correspondant.

### Codex a des hooks

Le binaire connaît `--dangerously-bypass-hook-trust` : il **exécute des hooks**,
mais ceux-ci exigent un « hook trust » persisté. C'est la voie pour obtenir un
`session_id` aussi fiable que celui de Claude, **sans** dépendre du pattern
d'écran. À explorer : la liste des événements exposés (Claude en a 8 ; codex en
expose au moins `Stop`).

### Le canal `set-status` est déjà générique

`tab-atelier set-status --kind <kind> --session <id> --state <état> --label <texte>`
existe et n'impose **aucune liste blanche de kinds**. Un wrapper autour de codex
peut donc déclarer kind, session, état et libellé **sans modifier tab-atelier**.
C'est le chemin le plus court pour les points 1 et 3 — et il ne coûte que le
lancement de codex via ce wrapper.

## 7. Reproduire ces constats

```bash
# Le canal d'état
grep -n "enum AgentState" -A 20 src/lib.rs
sed -n '25,70p' src/api/handlers/cards.rs

# Le point de branchement
sed -n '689,700p' src/lib.rs

# Les hooks qui alimentent l'état (côté machine)
python3 -c "import json;print(json.load(open('$HOME/.claude/settings.json'))['hooks'])"

# La session codex et son identifiant
ls ~/.codex/sessions/*/*/*/ | tail
jq -r 'select(.type=="session_meta")|.payload' ~/.codex/sessions/*/*/*/rollout-*.jsonl

# L'index des sessions, avec l'intitulé écrit par l'agent
cat ~/.codex/session_index.jsonl | jq -c .

# Le canal d'état générique (aucune liste blanche de kinds)
tab-atelier set-status --help
```
