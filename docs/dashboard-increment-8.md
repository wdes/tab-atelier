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
- Sous-commandes `set-*` déterministes (miroir `set-assignment`) + expo dans `DashboardTab`.
- **Fallback** : champs absents → vides, zéro régression Inc5/6/7.
- Web : rendre `objective` / `currentTask` / badge **« libre »** (orchestrator=free) sur la carte.
TDD : parse/round-trip des champs (pur) + accept écran.

### S2 — `free-bot` + enforcement déterministe [bash + hook]
- **`~/Dev/Botmox/free-bot.sh`** (modèle `spawn-bot.sh`, déterministe, 0 token agent) : pose/relâche
  l'assignment + les champs carte ; `free-bot <uuid> free` → déclare libre, retour bande Freelancers.
- **Hook BLOQUANT** (modèle G1 pre-push) : refuse une action d'agent sans assignment (one-time,
  quasi gratuit). PAS de nudge (récurrent, coûteux).
- **Check de fraîcheur** : FLAGGE sur le dashboard (lecture déterministe, **token-free**) un
  objectif/tâche périmé — PAS de dispatch auto (réservé au jugement de l'orchestrateur).

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
- `evalCriteria` co-définis (agent + orchestrateur), **validés par Olympe** (nouvel agent neutre,
  ni Joséphine/trust ni le sage/handbook).

### S5 — Process de libération (state machine) + boucle auto-amélioration
1. Travail terminé ? (libération orchestrateur + éval, pondérée par tokens).
2. Docs de travail rangés ? (utiles / supprimer / conserver, coop orchestrateur).
3. Auto-amélioration ? (coop MAS/tichef sur l'éval) → si oui : (a) consigner ancien prompt + éval,
   (b) modifier `specialty` + RAZ évaluation.
4. Handoff → auto-rehome → déclaration `free` → bande Freelancers.

## Ordre & intégration
S1 (carte base) → S2 (free-bot + hook) → S3 (clic droit + méta-trio) — cœur ; puis S4 (évals + Olympe),
S5 (libération). Web sur `feat/hd-web` (resync d'abord), rust sur `feat/harness-dashboard`. TDD strict
(coverage-first). Non-régression Inc5/6/7 verte. Intégration = merge `feat/hd-web` → `feat/harness-dashboard`,
puis `mx/live` au déploiement.
