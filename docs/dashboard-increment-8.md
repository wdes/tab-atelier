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
- **Méta = trio** : `tichef` (probabiliste) + **Brain** (déterministe anti-freeze) + **aligator**
  (assistant déterministe/gating). tichef remplit les cartes de Brain + aligator (daemons). Les
  afficher en Méta rend leur **statut GUI-visible + observable par les agents**.

### S4 — Évaluations + évaluateur « Olympe » [rust + web + nouvel agent]
- `evaluations[]` (permalog d'évals) : `{ evaluator, at, taskRef, tokens:{in,out},
  scores:{relevance,errors,omissions}, verdict, note }`. Exposé `/dashboard/state`.
- **Seuil** : max **1 erreur / 1 M tokens** (`errorRate = errors/(tokens.in+out)`). **2ᵉ erreur dans
  la fenêtre → déclenche l'auto-amélioration** (S5).
- **[G-c, garde-fou tichef] Fenêtre DÉTERMINISTE (reproductible)** : compteurs `(tokens, errors)`
  **depuis le dernier reset** = spawn OU dernière auto-amélioration (aligné sur la RAZ de C.3.b).
  Budget = **1 erreur par tranche de 1 M tokens entamée** dans l'époque ; déclenchement quand
  `errors` dépasse le budget (à <1 M tokens : budget=1 → la 2ᵉ erreur déclenche). Pas de fenêtre
  glissante (éviterait de stocker un stamp par erreur) — reset-époque, 2 compteurs, reproductible.
- `evalCriteria` co-définis (agent + orchestrateur), **validés par Olympe** (nouvel agent neutre,
  ni Joséphine/trust ni le sage/handbook).

### S5 — Process de libération (state machine) + boucle auto-amélioration
1. Travail terminé ? (libération orchestrateur + éval, pondérée par tokens).
2. Docs de travail rangés ? (utiles / supprimer / conserver, coop orchestrateur).
3. Auto-amélioration ? (coop MAS/tichef sur l'éval) → si oui : (a) consigner ancien prompt + éval,
   (b) modifier `specialty` + RAZ évaluation.
4. Handoff → auto-rehome → déclaration `free` → bande Freelancers.

## Ordre & intégration
S1 (carte base) → **S2a (free-bot, sûr)** → S3 (clic droit + méta-trio) — cœur ; puis **S2b (hook
bloquant, gate séparé — risque flotte, G-a)**, S4 (évals + Olympe), S5 (libération). Web sur
`feat/hd-web` (resync d'abord), rust sur `feat/harness-dashboard`. TDD strict (coverage-first).
Non-régression Inc5/6/7 verte. Intégration = merge `feat/hd-web` → `feat/harness-dashboard`, puis
`mx/live` au déploiement. **S2b ne merge pas sans validation explicite tichef/PO.**
