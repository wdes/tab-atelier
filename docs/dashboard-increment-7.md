# Dashboard — Increment 7 : organigramme compact + altitude dynamique + refresh sans flicker

> Contrat de l'incrément. Source de vérité pour le refiner (rouges) puis les builders (verts).
> S'appuie sur les données DÉJÀ posées en Inc5/Inc6 : `assignment`, `parent_tab_id`,
> `serving`, `services`/`repo_families`, `/dashboard/activity`. Contrat de langage : `docs/dashboard.md`.

## Intention (retour PO, mockup Excalidraw)

La disposition L0 actuelle prend trop de place. On veut un **organigramme compact en 4 bandes
d'altitude explicites**, où l'altitude = **niveau d'intervention potentiel de plus en plus global
en montant** — PAS une hiérarchie. Organisation **horizontale** : les agents bougent d'altitude
dynamiquement selon les besoins.

```
Méta            [ tichef ] [ planner idle ] [ refiner idle ]        ← seul tichef y est épinglé
- - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - -
Orchestrateurs  [ Kalpin ]        [ FX ]   [ CodeXplorer ] [ … ]
                  │   │  ╲                    (gère 1 repo)
                  ▼   ▼   ▼
                [K-Front][K-Back][K-etc]      ← repos servis (sous-nœuds de l'orchestrateur)
- - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - -
Workers         [Tab xv][Tab xx][Tab xy]   [Tab xz]        ← rattachés à leur repo (parent_tab_id)
- - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - -
Freelancers     [Tâche X][Tâche y][…]                       ← non-assignés (ex-"unassigned")
═══════════════════════════════════════════════════════════════
Indicateurs de suivi  (panneau activité "Dernières heures", compact, en bas)
```

## Sémantique de l'altitude dynamique (règle centrale)

- **Méta** = portée globale. **tichef seul y est épinglé** (il supervise les orchestrateurs).
- Un **spécialiste méta** (planner, refiner, auditeur…) qui **sert** une équipe — override `projet:`
  de son assignment / champ `serving` — **descend** dans la bande de cette équipe le temps de la
  mission, marqué "en renfort". Il ne flotte en Méta que sans mission d'équipe.
- Un **freelance** (non-assigné, bande Freelancers) qui reçoit un assignment **monte** rejoindre
  son orchestrateur/équipe.
- Les bandes ne sont pas une chaîne de commandement : c'est un **niveau d'intervention**. Le
  mouvement inter-bandes est la norme, pas l'exception.

## Slices

### S1 — Organigramme compact en 4 bandes (web)
Refondre le rendu L0 en 4 bandes empilées **Méta / Orchestrateurs / Workers / Freelancers**
(séparateurs pointillés + libellés) + le bandeau **Indicateurs** en bas. Chaîne à 3 étages :
orchestrateur → **repo(s) servis** (sous-nœuds) → **workers** (rattachés par `parent_tab_id`).
Un orchestrateur mono-repo pointe directement ses workers (cas FX). Boîtes compactes.
- **Fonction pure** `bandLayout(state)` → `{ meta[], orchestrators[{repo(s), workers}], freelancers[], activity }`.
- Fallback : état sans `services`/assignment → dégrade proprement (pas de crash, bande Freelancers pour les non-mappés).
- QUE `assets/dashboard.*`. Acceptance écran (Playwright) : 4 bandes visibles, chaîne orch→repo→worker rendue, compacité (hauteur < layout Inc6 sur le même état).

### S2 — Altitude dynamique (web)
Encoder les règles ci-dessus dans le placement :
- spécialiste méta avec `serving`/override `projet:` → rendu dans la bande de l'équipe servie (badge "renfort"), PAS en Méta ;
- freelance recevant un assignment → rendu sous son orchestrateur (plus en Freelancers) ;
- tichef → toujours Méta (épinglé, même s'il "sert").
- **Fonction pure** `resolveAltitude(tab, state)` → `meta|orchestrator|worker|freelancer` (+ team cible si renfort). TDD sur les 4 mouvements + le pin tichef.

### S3 — Refresh sans flicker (borrow Zoetrope, web)
Aujourd'hui le poll re-render tout → perte de sélection/scroll/hover + flicker. Adopter le
pattern Zoetrope (modèle idempotent, mutation en place) : **IDs de nœuds stables**, patch
DOM/SVG en place (pas de clear-and-rebuild), **sélection + scroll + hover + position préservés**
au refresh.
- **Fonction pure** `diffRender(prevModel, nextModel)` → liste d'opérations (add/update/remove) par id stable. TDD : (a) un tick sans changement structurel ne recrée aucun nœud, (b) sélection/scroll survivent, (c) ajout/retrait d'un tab = add/remove ciblé.

### S4 — Sous-agents & tâche courante par tab (rust + web)
Chaque carte affiche la **tâche courante** du tab + les **sous-agents `Task()` invoqués**, lus dans
son transcript `~/.claude/projects/*.jsonl` (même source que le scribe). Compact : sous-liste ou
chips dans/sous la carte.
- Rust/scribe : endpoint ou champ exposant, par tab, `currentTask` (dernier prompt/outil) + `subAgents[]` (nom + état). Réutilise la lecture JSONL du scribe. Ponytail acceptable : parse best-effort, tab sans transcript → champs vides sans erreur.
- Web : rendu chips/sous-liste. TDD pur sur l'extraction + le rendu.

### S5 — Minimap (web, CONDITIONNELLE)
Uniquement **si** le layout compact déborde encore l'écran sur la flotte réelle. Pattern simple :
un rect de viewport sur les bounds du graphe, cliquable/déplaçable (math pure, pas de lib).
- **Ne pas implémenter tant qu'on n'observe pas de débordement** après S1. À réévaluer sur mesure réelle (hauteur totale vs viewport). Noté conditionnel (YAGNI).

## Ordre & intégration
S1 → S2 → S3 (cœur, front) en priorité ; S4 (plumbing rust+web) ensuite ; S5 conditionnelle.
Web sur `feat/hd-web` (resync d'abord), rust sur `feat/harness-dashboard`. Intégration = merge
`feat/hd-web` → `feat/harness-dashboard` (assets disjoints), puis dans `mx/live` au déploiement.
TDD strict (refiner pose les rouges par slice, builder les rend verts, ping par slice).
Non-régression : Inc5/Inc6 accept + I2 restent verts (org-chart Inc6 = fallback si pas de bandes).
