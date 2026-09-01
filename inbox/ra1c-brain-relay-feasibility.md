# Note de faisabilité — brain-relay PUSH vs crons de ronde PULL (RA1c)

> Question MAS : Brian push+submit sur événement peut-il remplacer les crons de ronde
> PULL par du PUSH fiable ? — évaluation courte du builder RA1c.

## La brique est déjà là
RA1c a factorisé `push_swamp_input(target, msg, submit, at, priority, dedup)` — un
helper **générique** : sur un événement, POUSSE du texte à une cible ET (si `submit`)
presse Entrée pour **déclencher le tour** d'une cible idle. Il passe par le chemin
`swamp → aligator`, donc il hérite **gratuitement** de la régulation d'aligator :
rate-cap par round, transient-retry des tabs pas-encore-live, dedup par clé. C'est
exactement ce qu'un push-relay Brian demande. Zéro nouvelle plomberie à écrire.

## Feasible — oui, sous conditions
- **Latence** : PUSH event-driven bat le PULL périodique (pas d'attente du prochain
  tick de cron). ✅
- **Fiabilité** : le chemin swamp→aligator est **best-effort régulé**, pas
  exactly-once. MAIS RA1c garde la **note ops durable (fallback PULL)** immédiate → un
  PUSH raté est rattrapé par une ronde. `PUSH + note-fallback` = assez fiable pour
  remplacer un cron. ✅ (avec le filet)
- **Anti-herd** : `wake_schedule` (round-robin + gap fixe) **généralise** — Brian peut
  étaler N pushes multi-cibles de la même façon. ✅
- **Le vrai verrou = la DÉTECTION d'événement** : un cron est auto-contenu (il poll sur
  timer). Pour le remplacer, **Brian doit détecter de façon fiable les événements que le
  cron poll aujourd'hui**. Si Brian rate un événement, il n'y a pas de push → le cron le
  couvrait. C'est là que se joue le remplacement, pas dans le transport (le transport
  est résolu par RA1c). ⚠️

## Recommandation
PUSH-relay via Brian = **feasible comme remplaçant de cron** pour les événements que
Brian détecte, en réutilisant `push_swamp_input(submit=true)` + le fallback-note durable
+ le stagger anti-herd. **Garder les crons en fail-safe backstop** jusqu'à ce que (a) la
fiabilité PUSH soit prouvée en vrai (RA1c déployé + mesuré) ET (b) la couverture de
détection d'événement de Brian égale ce que les crons poll. C'est exactement la séquence
de la roadmap : **RA2 = désarmer les crons redondants APRÈS RA1c prouvé** — ne pas
inverser l'ordre. Le transport PUSH fiable est livré (RA1c) ; la bascule cron→push est un
gate séparé conditionné à la preuve terrain + à la détection d'événement.
