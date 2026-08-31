# Playbook — Équipe d'agents auto-organisée (test de la comms fabric)

> Rédigé par Primitive-Auditor (auteur de l'audit des primitives + reviewer de la primitive `task` S1-S4).
> But : **prompts prêts à copier-coller** pour monter une équipe ad-hoc (orchestrateur + reviewer + builders, + appel d'experts) qui **s'auto-organise** autour de la nouvelle primitive de coordination `task`. Réfs : [`comms-fabric-inventory.md`](./comms-fabric-inventory.md), [`primitive-audit.md`](./primitive-audit.md), [`issue-task-queue-lease.md`](./issue-task-queue-lease.md).

---

## 0. Principe d'auto-organisation (le « pourquoi »)

L'équipe **ne se micro-assigne pas**. L'orchestrateur **remplit un POOL de tâches** (`task push`) ; les builders **se servent** (`task claim`) selon leur capacité, sans qu'un chef leur attribue nominativement le travail. C'est le **3ᵉ mode de coordination** — *pull-répartition atomique* — qui complète les deux existants :

| Mode | Outil | Usage |
|---|---|---|
| **Broadcast** (diffusion) | `note`/`notes` (blackboard) | statuts, verdicts, annonces, journal auditable |
| **Push ciblé** (je te livre) | `dispatch --to`, `swamp`+`aligator` | ordre nominatif à UN agent précis |
| **⭐ Pull-répartition atomique** (je pioche) | **`task` (nouveau)** | un pool où N pairs idle piochent sans course ni chef |

Le gain : **robuste au lieu de poli**. Deux builders qui claiment la même tâche → le daemon sérialise, **exactement un** gagne. Un builder qui meurt mid-tâche → son *lease* expire, la tâche **retombe dans le pool** (récup d'orphelin gratuite).

---

## 1. Inventaire des outils de comms (la fabric)

### ⭐ `task` — répartition PULL atomique (la primitive à tester)
```
task push  --queue <q> [--to <role>] [--priority N] "<payload>"   # enfile (orchestrateur)
task claim --queue <q> [--as <role>] [--lease <secs>]             # pioche atomique → {id,payload,lease_until} | vide
task beat  <task-id> [--lease <secs>]                             # renouvelle mon lease (tâche longue)
task done  <task-id>                                             # complète (exactly-once)
task list  --queue <q>                                           # read-model (qui a quoi) — READ-ONLY
```
**Sémantique à connaître (gotchas §Annexe) :**
- **Lease 35 min par défaut.** Une tâche longue **doit** `beat` avant l'expiry, sinon elle est ré-attribuée → **at-least-once en exécution** : *rends ton travail idempotent ou tolère un re-run*. Seule la **complétion** est exactly-once.
- **`--to <role>` = HINT, pas FENCE.** Le rôle est self-déclaré (carte `assignment`/`specialty`) → routage advisory, **pas** de l'autorisation. Un pair honnête mal-routé est stoppé ; un menteur non. (Pour une équipe coopérative : parfait.)
- **Ownership = tab-id automatique.** Seul le tab qui a claimé peut `beat`/`done` (les autres → 409 stale). Tu n'as rien à déclarer, le daemon connaît ton tab.
- **Pas de `task fail` (encore).** Pour rendre une tâche sans la faire : laisse le lease expirer (≤35 min) — elle revient au pool. (Follow-up connu.)

### Blackboard — broadcast durable
```
note  <msg> --topic <t> --from <who>      # poste (broadcast, append-only, lisible par tous + Brain + Olympe)
notes --topic <t> [--since <N>]           # lit (polling incrémental via --since)
```
Convention de **ping adressé** : 2 lignes vides puis le message **entre double-crochets** `[[ … ]]`.

### Observabilité
```
peers [--all]          # UNE ligne/tab : nom · état · cwd · contexte — triage flotte (qui est busy/idle)
peek <tab> [--lines N] # lire l'écran d'un pair (le détail)
tabs --json            # dump structuré + carte agent (assignment, context_pct…)
task list --queue <q>  # état du pool (queued/claimed@peer/done)
```
Règle : **`peers` pour l'état, `peek` pour le détail, `task list` pour le pool.**

### Livraison de fichier / handoff
```
handoff <file> <tab>   # dépose un fichier dans l'inbox/ d'un pair (handoff lourd, hors-bande)
```

### Push nominatif direct
```
dispatch --to <tab> "<prompt>" [--wait] [--quiet <s>] [--timeout <s>]   # ordre à UN tab (+ --wait = attends idle + report)
dispatch --new --name <n> --cwd <d> --cmd "<launcher>" "<prompt>"        # crée un tab neuf + agent
swamp <tab-uuid> "<txt>" [--from <who>]                                  # enfile pour livraison RÉGULÉE par aligator (anti-herd)
```

### Carte agent (état déclaré, alimente le dashboard)
```
set-assignment "[<projet>:]<phase>/<role>"   # rôle stable, hook-immune (ex. "kalpin-back:build/builder")
set-specialty  "<spécialité>"                # spécialité câblée
set-current-task "<phrase>"                  # tâche courante (pill dashboard)
set-status <idle|thinking|waiting|error>     # état (posé par les hooks, manuel si besoin)
```

### Hygiène / liveness (daemons)
```
clarify <tab>          # refresh de contexte IN PLACE (re-home) d'un tab saturé
clarify --watch        # poller auto (>90% contexte)
brain                  # daemon anti-freeze (nudge 'continue' aux agents coincés)
aligator               # daemon routeur (draine la file swamp, livraison régulée)
```

### Wrappers custom (poste)
```
~/Dev/Botmox/spawn-bot.sh <name> <cwd> "<prompt>" [assignment] [currentTask]   # spawn agent en mode auto + carte
~/Dev/Botmox/dispatch-task.sh <uuid> "<tâche>" [-- <args dispatch>]            # set-current-task + dispatch atomiques
~/Dev/Botmox/rehome-tab.sh <uuid> <repo> "<phase/role>" <nom> [--go]           # relocalise/refresh un tab avec handoff
```

---

## 2. Le flux d'auto-organisation

```
                    ┌─────────────────────────────────────────────┐
   OBJECTIF (PO) ──▶ │ ORCHESTRATEUR                               │
                    │  • décompose en tâches                       │
                    │  • task push --queue build --to builder …    │──┐  POOL "build"
                    │  • spawn builders + reviewer (spawn-bot.sh)  │  │  [t1][t2][t3]…
                    │  • monitore: task list / peers / notes       │  │
                    └───────────────▲─────────────────────────────┘  │
                                    │ verdicts/statuts (blackboard)   │ claim --as builder
                                    │                                 ▼
   ┌──────────────┐  push review   ┌─────────────────────────────────────────┐
   │ REVIEWER     │◀───────────────│ BUILDERS (pool, N pairs)                 │
   │ claim review │  (résultat)    │  loop: claim build → beat → work → done  │
   │ verdict 🟢/🔴│───────────────▶│        → push review (le résultat)       │
   │ 🔴→requeue   │  POOL "review" └─────────────────────────────────────────┘
   └──────┬───────┘  [r1][r2]…
          │ appel à la demande
          ▼
   EXPERTS HABITUELS (Olympe=verdict, code-reviewer, russell=fact-check, /audit, /pre-merge-check…)
```

**Cycle de vie d'une tâche :** `push(build)` → `claim(builder)` → `beat*` → work → `done` + `push(review)` → `claim(reviewer)` → verdict → 🟢 `done(review)` **ou** 🔴 `push(build, priority haute)` + `done(review)`. L'orchestrateur voit les deux pools se vider via `task list`.

---

## 3. Prompts de base PAR RÔLE (copy-paste)

> Remplace `<MISSION>`, `<REPO/CWD>`, `<OBJECTIF>`. Chaque prompt est autoportant : l'agent connaît son rôle, ses commandes, et la discipline.

### 3.1 — ORCHESTRATEUR (le bootstrap d'auto-organisation)

```
Tu es l'ORCHESTRATEUR de l'équipe "<MISSION>". Assignment: "<MISSION>:build/orchestrator".
Pose ta carte: tab-atelier set-assignment "<MISSION>:build/orchestrator" ; set-status thinking.

OBJECTIF: <OBJECTIF>. Repo: <REPO/CWD>.

Tu NE codes PAS. Ton job = décomposer, remplir le pool, spawner l'équipe, monitorer, clôturer.

1. DÉCOMPOSE l'objectif en 3 à 6 tâches INDÉPENDANTES et idempotentes (chacune re-exécutable sans casse — le lease peut ré-attribuer). Pour chacune, un payload CONCIS et autoportant (quoi faire + critère de done + fichiers concernés).

2. REMPLIS LE POOL (une commande par tâche):
   tab-atelier task push --queue build --to builder --priority <0-9> "<payload autoportant>"
   (priorité haute = plus urgent ; par défaut 0). Vérifie: tab-atelier task list --queue build

3. SPAWNE l'équipe (2-3 builders + 1 reviewer) via le wrapper (mode auto + carte):
   ~/Dev/Botmox/spawn-bot.sh "Builder-1" <REPO/CWD> "<colle le PROMPT BUILDER 3.2>" "<MISSION>:build/builder"
   ~/Dev/Botmox/spawn-bot.sh "Builder-2" <REPO/CWD> "<PROMPT BUILDER>" "<MISSION>:build/builder"
   ~/Dev/Botmox/spawn-bot.sh "Reviewer"  <REPO/CWD> "<colle le PROMPT REVIEWER 3.3>" "<MISSION>:review/reviewer"

4. ANNONCE le départ sur le blackboard:
   tab-atelier note --topic <MISSION> --from orchestrator "Pool build rempli (<N> tâches). Builders + reviewer lancés. Piochez: task claim --queue build --as builder."

5. MONITORE en boucle (toutes les ~2-3 min, sans herd):
   - tab-atelier task list --queue build   (combien queued/claimed/done)
   - tab-atelier task list --queue review
   - tab-atelier peers                      (qui est busy/idle/error)
   - tab-atelier notes --topic <MISSION> --since <dernier-index>   (rapports + verdicts)
   RÉ-ALIMENTE si un verdict 🔴 a re-poussé une tâche de fix ; ré-veille un builder idle par un note si le pool a du stock.

6. CLÔTURE: quand build ET review sont vides (que des 'done'), poste un récap sur le blackboard et set-status idle. Ne merge/deploy RIEN sans le PO.

DISCIPLINE: ping-back sur le blackboard à chaque étape clé (format: 2 lignes vides puis [[ … ]]). Si un tab dépasse ~90% de contexte, propose un refresh: tab-atelier clarify <tab>.
```

### 3.2 — BUILDER (worker de pool)

```
Tu es un BUILDER de l'équipe "<MISSION>". Assignment: "<MISSION>:build/builder".
Pose ta carte: tab-atelier set-assignment "<MISSION>:build/builder".

Tu travailles en PULL depuis un pool partagé. BOUCLE:

1. PIOCHE une tâche:
   tab-atelier task claim --queue build --as builder --lease 1800
   - Réponse VIDE (rien à l'écran / code 204) → pool épuisé: set-status idle, attends ~60s, re-tente. Si 3 fois vide → annonce-toi disponible (note) et stand-by.
   - Réponse {id,payload,lease_until} → tu OWN cette tâche (par ton tab-id, personne d'autre ne peut la done).

2. DÉCLARE + bosse:
   tab-atelier set-current-task "<résumé de la tâche>" ; set-status thinking
   Fais le travail décrit dans le payload. IDEMPOTENT: si la tâche a peut-être déjà été partiellement faite (re-run après expiry), vérifie l'état avant d'agir.

3. TÂCHE LONGUE (> ~25 min): renouvelle ton lease AVANT l'expiry sinon la tâche t'est retirée:
   tab-atelier task beat <id> --lease 1800     (toutes les ~15-20 min)

4. TERMINE:
   - Complète la tâche: tab-atelier task done <id>
   - Passe le RÉSULTAT à la review (nouvelle tâche dans le pool review):
     tab-atelier task push --queue review --to reviewer "REVIEW <MISSION>: <ce que j'ai fait> — diff/branche/fichiers: <refs>. Critère: <comment vérifier>."
   - Rapporte: tab-atelier note --topic <MISSION> --from builder "DONE <id>: <1 ligne>. Passé en review."

5. RECOMMENCE à l'étape 1 jusqu'à pool vide.

BLOCAGE: si tu es coincé (dépendance manquante, ambigu), NE bloque pas la tâche indéfiniment: poste sur le blackboard (note --topic <MISSION> --from builder "BLOQUÉ <id>: <quoi>") et laisse le lease expirer (la tâche revient au pool) OU attends une réponse de l'orchestrateur. Pas de `task fail` pour l'instant.
```

### 3.3 — REVIEWER

```
Tu es le REVIEWER de l'équipe "<MISSION>". Assignment: "<MISSION>:review/reviewer".
Pose ta carte: tab-atelier set-assignment "<MISSION>:review/reviewer".

Tu consommes le pool "review" en PULL. BOUCLE:

1. PIOCHE une review:
   tab-atelier task claim --queue review --as reviewer --lease 1800
   - Vide → set-status idle, attends ~60s, re-tente.
   - {id,payload} → tu OWN cette review.

2. REVIEW le résultat décrit dans le payload (diff, branche, fichiers). Pour un avis d'EXPERT, invoque un agent habituel (§4 "appel expert"): code-reviewer pour le diff, russell pour fact-check, /pre-merge-check pour un verdict consolidé, Olympe pour un verdict d'éval.

3. VERDICT sur le blackboard (traçable):
   tab-atelier note --topic <MISSION> --from reviewer "VERDICT <id>: 🟢 GO | 🟡 réserves | 🔴 à corriger — <justification courte + file:line>."

4. ROUTE:
   - 🟢/🟡 acceptable → tab-atelier task done <id>
   - 🔴 → re-pousse une tâche de FIX dans le pool build (priorité haute) PUIS clôture la review:
     tab-atelier task push --queue build --to builder --priority 8 "FIX <MISSION>: <ce qui ne va pas + où>. Re-review après."
     tab-atelier task done <id>

5. RECOMMENCE jusqu'à pool review vide.

RÈGLE: verdict indicatif, jamais impératif. Aucun merge/deploy. Tu diagnostiques, l'humain/PO tranche.
```

### 3.4 — EXPERT (invocation à la demande)

```
[Spawn d'un expert ponctuel par l'orchestrateur ou le reviewer]

tab-atelier dispatch --new --name "Expert-<type>" --cwd <REPO/CWD> --cmd "claude --permission-mode auto" \
  "Tu es <type> (code-reviewer | russell fact-check | coherence-reviewer | test-coverage-reviewer | Olympe verdict). \
   Mission ponctuelle: <ce qu'on te demande, refs précises>. \
   Rends ton verdict sur le blackboard: tab-atelier note --topic <MISSION> --from <type> \"[[ <verdict structuré> ]]\". \
   READ-ONLY strict, aucun changement de code. Stand-by après le verdict." --wait --quiet 30

# Alternative: si l'expert est un slash-command kalpin, l'orchestrateur le lance dans son propre tour:
#   /pre-merge-check !<MR>   ·   /audit   ·   /russell !<MR>
```

---

## 4. Prompts PAR CAS (snippets réutilisables)

**Spawner un membre (mode auto + carte posée automatiquement) :**
```
~/Dev/Botmox/spawn-bot.sh "<Nom>" "<CWD>" "<prompt de rôle>" "<projet>:<phase>/<role>" "<tâche initiale>"
```

**Handoff lourd (passer un gros artefact à un pair via son inbox/) :**
```
tab-atelier handoff /chemin/vers/rapport.md <tab-cible>
# puis préviens-le: tab-atelier note --topic <MISSION> --from <moi> "handoff: rapport.md dans ton inbox/"
```

**Débloquer un agent coincé (nudge régulé) :**
```
tab-atelier peek <tab> --lines 30                 # diagnostique d'abord
tab-atelier swamp <tab-uuid> "reprends: <consigne>" --from orchestrator   # livraison régulée (aligator)
# ou direct si urgent: tab-atelier dispatch --to <tab> "<consigne>"
```

**Attendre le résultat d'un agent (synchrone) :**
```
tab-atelier dispatch --to <tab> "<demande>" --wait --quiet 20 --timeout 300   # rend l'écran quand il redevient idle
```

**Refresh de contexte d'un tab saturé (>90%) :**
```
tab-atelier clarify <tab>          # re-home IN PLACE (même cwd/rôle/nom), préserve le fil via handoff
```

**Recovery post-restart du daemon :** le `restart-watcher` (cron) réveille les orchestrateurs et relance aligator ; sinon, à la main :
```
tab-atelier notes --topic ops --since <N>   # cherche 'RESTART_DONE'
# ré-arme: re-annonce le pool sur le blackboard, vérifie task list (les tâches survivent au restart, elles sont sur disque).
```

---

## 5. Cheat-sheet par rôle

| | Orchestrateur | Builder | Reviewer |
|---|---|---|---|
| **Alimente** | `task push --queue build` | `task push --queue review` | `task push --queue build` (fix 🔴) |
| **Consomme** | `task list` (monitor) | `task claim --queue build --as builder` | `task claim --queue review --as reviewer` |
| **Maintient** | `peers`, `notes` | `task beat <id>` (tâche longue) | — |
| **Clôt** | récap blackboard | `task done <id>` | `task done <id>` + verdict `note` |
| **Carte** | `<M>:build/orchestrator` | `<M>:build/builder` | `<M>:review/reviewer` |

---

## 6. Scénario de test de l'équipe ad-hoc (bootstrap)

Pour éprouver la fabric, une première mission **petite, décomposable, à faible risque** :

1. **Choisir** un objectif à 3-4 tâches indépendantes (ex. « ajouter 4 petits helpers + leurs tests unitaires dans `<repo>` », ou « corriger 3 typos/refs + 1 lint dans `<repo>` »).
2. **Lancer l'orchestrateur** (prompt §3.1) avec cet objectif → il pousse 3-4 `task push`, spawne 2 builders + 1 reviewer.
3. **Observer la fabric en action** :
   - `task list --queue build` : les tâches passent `queued → claimed@<peer> → done` (dérivé au read).
   - Provoquer un **test d'orphelin** : tuer/fermer un builder mid-tâche → vérifier que sa tâche (lease expiré ~35 min, ou `--lease 60` pour tester vite) **retombe au pool** et qu'un autre builder la reprend.
   - Provoquer un **test de course** : lancer 2 builders sur un pool à 1 tâche → vérifier qu'**un seul** l'obtient (l'autre a 204).
   - Vérifier la **boucle review** : un 🔴 re-pousse un fix, un 🟢 clôture.
4. **Lire les verdicts** sur `notes --topic <MISSION>` + le dashboard (`/dashboard/state` expose la section `tasks`).
5. **Bilan** : la coordination a-t-elle tenu sans micro-management ? Les orphelins ont-ils été récupérés ? → itérer les prompts.

**Astuce test rapide** : pour ne pas attendre 35 min, claim avec un lease court `--lease 60` (60 s) pour observer la ré-attribution d'orphelin en direct.

---

## Annexe — Gotchas (à connaître avant de tester)

- **Idempotence obligatoire** : le lease donne *at-least-once en exécution*. Une tâche peut re-tourner (builder mort → réattribution). Le payload doit être re-exécutable sans casse (vérifier l'état avant d'agir).
- **`beat` ou perds ta tâche** : au-delà du TTL (35 min déf.) sans `beat`, ta tâche est reclaimable → un autre la prend. `beat` toutes les ~15-20 min sur les tâches longues.
- **`done` exige un lease vivant** : un `done` tardif (lease expiré / tâche ré-attribuée) → 409 stale. C'est voulu : tu ne peux pas clore le travail d'un autre.
- **`--to`/`--as` = HINT advisory, pas une barrière de sécurité** : rôle self-déclaré (carte). Suffisant pour une équipe coopérative ; ce n'est PAS de l'autorisation. Seul l'**ownership** (tab-id) est dur.
- **Pas de `task fail`** : pour rendre une tâche non faite, laisse le lease expirer. (Verbe de relâche volontaire = follow-up connu.)
- **Les tâches survivent au restart** (sur disque, `<state>/tab-atelier/tasks/<queue>.jsonl`) ; les daemons `aligator`/`brain` sont relancés au restart (durabilité native).
- **`peers` d'abord, `peek` ensuite** : triage éco-contexte (1 ligne/tab) avant de dumper un écran.
- **Ping-back discipliné** : format `[[ … ]]` après 2 lignes vides sur `--topic`, pour que Brain/Olympe/l'orchestrateur suivent.

---

*Prêt à instancier une équipe ad-hoc. Dis-moi la mission de test (repo + objectif) et je génère les prompts remplis + la séquence de spawn.*
