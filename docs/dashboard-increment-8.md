# Dashboard — Increment 8 : carte d'agent vivante (assignment++) + libération/auto-amélioration + méta-trio

> Contrat de l'incrément (design complet : issue a-biskoazh/tab-atelier-mx#4). Fait de l'`assignment`
> un **pivot d'identité vivante** : au-delà de `role`, une « carte d'agent » auto-déclarée ET observée.
> Sert les 2 caps : **interaction inter-agents** + **inter-observabilité**. S'appuie sur le double
> mécanisme déclaré↔observé (l'agent DIT / le daemon VOIT).

## Principe
`assignment` reste le **pivot d'identité**. On lui ajoute des **champs frères persistants hook-immune**
(la « carte d'agent »), tous déterministes en lecture/écriture, tous exposés dans `/dashboard/state`
→ tous **observables** par les pairs et le dashboard.

## Slices

### S1 — Carte d'agent (champs de base) [rust + web]
Champs additionnels au `TabState` (persistants, hook-immune, camelCase dans `/dashboard/state`) :
- `specialty` (prompt/spécialité inscrit dans le dur), `orchestrator` (uuid ou `free`),
  `objective` (objectif courant), `currentTask` (**permalog** : une phrase, appendée → mémoire longue
  token-free relisible à la demande).
- **[G-b, garde-fou tichef] BORNER le permalog** : `currentTask` = **ring des N dernières entrées**
  (défaut ~50, configurable) OU cap taille — sinon `TabState` + `/dashboard/state` gonflent sans fin
  (même leçon que la compaction aligator). L'entrée courante + un historique borné, exposé borné.
- **[ajout PO] `roundsActive`** : indique si des **rondes (crons)** sont actives pour un orchestrateur —
  bool + horodatage `lastRoundAt` (ou compteur). Posé déterministe via `set-rounds-active` (miroir des
  `set-*`), exposé `/dashboard/state`. **Le champ + set-* + expo = S1** ; le rendu **pastille = S3**
  (vert = rondes actives / gris = aucune). Wiring : les crons de ronde (watcher/sage) appellent
  `set-rounds-active` à chaque tic (petite intégration S1/S2a). Sert l'inter-observabilité (voir
  d'un coup d'œil qui est supervisé).
- Sous-commandes `set-*` déterministes (miroir `set-assignment`) + expo dans `DashboardTab`.
- **Fallback** : champs absents → vides, zéro régression Inc5/6/7.
- Web : rendre `objective` / `currentTask` / badge **« libre »** (orchestrator=free) sur la carte.
TDD : parse/round-trip des champs (pur) + accept écran.

### S2a — `free-bot.sh` (sûr) [bash]
- **`~/Dev/Botmox/free-bot.sh`** (modèle `spawn-bot.sh`, déterministe, 0 token agent) : pose/relâche
  l'assignment + les champs carte ; `free-bot <uuid> free` → déclare libre, retour bande Freelancers.
- **Check de fraîcheur** : FLAGGE sur le dashboard (lecture déterministe, **token-free**) un
  objectif/tâche périmé — PAS de dispatch auto (réservé au jugement de l'orchestrateur).

### S2b — Hook BLOQUANT (slice à RISQUE, gate séparé) [hook]
> **[G-a, garde-fou tichef — CRITIQUE]** La slice la plus dangereuse : un hook fail-CLOSED sur SON
> PROPRE bug = **freeze flotte entière** (on sort à peine du brain-freeze). Exigences NON négociables :
- **(1) FAIL-OPEN sur sa propre erreur** : si le check d'assignment lui-même échoue → **laisse passer**,
  ne bloque JAMAIS sur un bug interne. (défaut = autoriser.)
- **(2) Opt-out env** : une var (modèle `KALPIN_ROOT_SESSION` block→ask) désarme/adoucit le hook.
- **(3) Scope PRÉCIS** : refuser une **action d'agent sans assignment**, PAS chaque appel d'outil.
- Modèle G1 pre-push (one-time, quasi gratuit). PAS de nudge (récurrent, coûteux).
- **Gate séparé** : ne pas merger S2b sans validation explicite (tichef/PO) — risque flotte.

### S3 — Vue « carte d'agent » au clic droit + méta-trio [web]
- **Clic droit sur un agent** → affiche sa **carte complète** (specialty, orchestrator, objective,
  currentTask, evaluations, evalCriteria). Orchestrateurs = carte aussi.
- **Pastille `roundsActive`** [ajout PO] sur la carte de l'orchestrateur : **vert** = rondes actives /
  **gris** = aucune (champ posé en S1).
- **Méta = trio** : `tichef` (probabiliste) + **Brain** (déterministe anti-freeze) + **aligator**
  (assistant déterministe/gating). tichef remplit les cartes de Brain + aligator (daemons). Les
  afficher en Méta rend leur **statut GUI-visible + observable par les agents**.

### S4 — Évaluations + évaluateur « Olympe » [rust + web + nouvel agent]
- `evaluations[]` (permalog d'évals, **schéma VALIDÉ PO**) : `{ evaluator, at, taskRef, tokens:{in,out},
  scores:{relevance,errors,omissions}, verdict, note }`. Exposé `/dashboard/state`.
- **Seuil** : max **1 erreur / 1 M tokens** — c'est une **moyenne** ; déclenche l'auto-amélioration (S5)
  sur (a) dépassement de moyenne OU (b) **burst ≥3 erreurs dans le dernier 1 M** (cf G-c).
- **[G-c + précision PO] DEUX déclencheurs déterministes (l'un OU l'autre)** :
  - **(a) MOYENNE** (le seuil est une moyenne) : compteurs `(tokens, errors)` **depuis le dernier
    reset** (spawn OU dernière auto-amélioration, aligné RAZ C.3.b). Budget = **1 err / 1 M tokens
    entamé** ; déclenche quand `errors` dépasse le budget (à <1 M : budget=1 → 2ᵉ erreur déclenche).
  - **(b) BURST** : **≥ 3 erreurs dans le dernier 1 M tokens** → auto-amélioration aussi. Fenêtre
    glissante récente **lue depuis les records `evaluations[]`** (qui portent déjà `tokens.{in,out}`)
    → pas de stockage neuf, reproductible.
  Reproductible dans les deux cas (compteurs + lecture du permalog borné d'évals).
- `evalCriteria` co-définis (agent + orchestrateur), **validés par Olympe** (nouvel agent neutre,
  ni Joséphine/trust ni le sage/handbook).
- **[ajout PO] champ GÉNÉRIQUE d'observabilité** `usageCount` (int) + `lastUsedAt` (timestamp) —
  versant **observé** de la carte (tout agent). Déterministe, expo camelCase (skip si None, zéro-reg).
  Sous-commande légère **`bump-usage <tab>`** (incrémente + timestamp, miroir set-*). **WIRING (PO)** :
  **brain** bump à CHAQUE `continue` émis (on saura enfin quand/combien de nudges) ; **aligator** bump
  à CHAQUE livraison depuis la swamp. But : rendre l'activité des daemons **observable** + alimenter le
  panneau « Dernières heures ». Enrichit aussi les cartes Brain/aligator (inc8-cards).
- **[ajout PO — fold APRÈS S4-vert] champ `conventions`** (versant **déclaré**, pendant de `usage`) :
  **liste libre** des `.md` de convention que l'agent DÉCLARE respecter (déclarés dans son prompt de
  base). Pose déterministe `set-conventions <tab> "handbook.md,quiesce-no-thundering-herd.md,…"`
  (miroir set-*), expo camelCase, rendu dans la carte + **flag si VIDE** (agent sans conventions
  déclarées — cas Bot Orc fan-out). **Validation = `ta-convention-auditor`** (déjà en Méta) : croise
  *déclaré* vs *existant* vs *comportement*. Design retenu : **liste libre** (write simple) + auditeur
  pour la validation sémantique (pas d'enum couplé au write).

### S5 — Process de libération (state machine) + boucle auto-amélioration
1. Travail terminé ? (libération orchestrateur + éval, pondérée par tokens).
2. Docs de travail rangés ? (utiles / supprimer / conserver, coop orchestrateur).
3. **Sur déclenchement (dès la 2ᵉ erreur / burst), l'orchestrateur DÉCIDE** — informé par la **taille
   de la fenêtre de contexte** de l'agent (observée) :
   - **REHOME d'abord** si le contexte est **gros/dégradé** → rafraîchit le contexte ; peut suffire
     (erreurs dues à la dégradation, pas au prompt). Moins coûteux que réécrire le prompt. Puis ré-évaluer.
     **[REUSE §18, tichef] Réutiliser `~/Dev/Botmox/rehome-tab.sh`** (chaîne complète handoff-written →
     successor-ready → ack → safe-to-close, `--auto-close`, swap nom, set-parent, warning crons) avec
     **assignment/name/cwd préservés** = l'agent renaît frais **sans perdre son identité** (specialty/
     objective se re-posent via les `set-*` de la carte S1). **Aucun nouveau code rehome.**
   - **AUTO-AMÉLIORATION** (coop MAS/tichef sur l'éval) si c'est un vrai problème de capacité/prompt :
     (a) consigner ancien prompt + éval, (b) modifier `specialty` + RAZ évaluation.
4. Handoff → auto-rehome → déclaration `free` → bande Freelancers. **[terrain tichef]** co-câbler la
   state-machine de libération sur `rehome-tab.sh` **avec tichef (producteur)** à S5 — pas de réinvention.

## Ordre & intégration
S1 (carte base) → **S2a (free-bot, sûr)** → S3 (clic droit + méta-trio) — cœur ; puis **S2b (hook
bloquant, gate séparé — risque flotte, G-a)**, S4 (évals + Olympe), S5 (libération). Web sur
`feat/hd-web` (resync d'abord), rust sur `feat/harness-dashboard`. TDD strict (coverage-first).
Non-régression Inc5/6/7 verte. Intégration = merge `feat/hd-web` → `feat/harness-dashboard`, puis
`mx/live` au déploiement. **S2b ne merge pas sans validation explicite tichef/PO.**
