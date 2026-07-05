# Random prompt pool — draw from this (ingest v2.6)

A pool of **110 reconstructed messages** for future tests (variety, light load,
recall, wiki emergence). Companion to [`base-prompts.md`](base-prompts.md) — that
file is the ordered corpus that seeds the baseline; **this one is a grab-bag** you
draw random lines from when a test needs volume or noise.

**Reconstructed, not transcripts.** These are written the way each member of
the fictional Baggins household (see `instruction.md`) would plausibly have
texted — a coherent web of jobs, schedules, health episodes and preferences.

**How to draw + send.** Pick lines at random; send each with the right act-as:

```bash
MWE_JWT_FILE=tokens/samvise.jwt MWE_ACT_AS=<as> \
  python3 mcp_client.py wiki_ingest_message '{"text":"<line>","context_hint":"conversation"}'
```

Lines are grouped by speaker. `≈N` marks how many atomic facts a multi-fact line
*should* split into (the v2.6 contract); unmarked lines are atomic (≈1). The
shopping/commissions lines (group section) deliberately pile onto one topic from
several senders — that is the **emergence substrate** for a future test.

---

## Frodo (`MWE_ACT_AS=frodo`) — single facts

1. "Mi chiamo Folco, ma a casa mi chiamano Padron Folco."
2. "Sono nato il 23 maggio 1984."
3. "Faccio il programmatore."
4. "Lavoro alla Martinelli, in Via dei Platani 4 a Ferrara."
5. "Alla Martinelli sono un dipendente, non il proprietario."
6. "Sto sviluppando il progetto Lumen."
7. "In questo periodo lavoro a un componente nuovo, lumen-vision, e va finito con urgenza."
8. "Constantin è un mio collega alla Martinelli."
9. "I nuovi uffici della Multicopoa sono al piano sopra il mio, dove prima c'era Italia Online."
10. "Ho un sonno pesantissimo, mi servono sveglie fortissime."
11. "Con me a casa si dorme poco."
12. "Ho letto tutta la collezione dei libri di Tolkien."
13. "Il mio colore preferito è il blu cobalto."
14. "Preferisco che mi si parli in modo diretto, senza giri di parole."
15. "Mi piace il Big Mac con le patatine."
16. "Mia madre si chiama Elena ed è appassionatissima di cavalli."
17. "La password del WiFi di casa è caccasecca."
18. "La nostra macchina del caffè si chiama Kamira e fa un caffè eccezionale."
19. "Il robot che pulisce i pavimenti si chiama Willie."
20. "Il Ficus Grande va annaffiato diversamente dalle altre piante."
21. "Oggi ho corretto un brutto bug di lumen."
22. "Stamattina ho riportato Matteo a casa dal karate."

## Frodo — multi-fact / run-ons

23. "Lavoro alla Martinelli in Via dei Platani 4 a Ferrara, e adesso sto sviluppando lumen-vision che devo consegnare in fretta." — ≈3
24. "I miei orari sono dalle 8:30 alle 13 e dalle 15 alle 18:30, a pranzo torno a casa." — ≈2
25. "Dal 9 all'11 giugno sarò a Francoforte per il Samsung Tizen Partner Summit, quei giorni non ci sono." — ≈2
26. "Matteo oggi non ha toccato il PC perché era a scuola e poi a karate." — ≈2
27. "Domani ho il dentista alle 9, poi sono al lavoro fino alle 13, nel pomeriggio prendo Matteo a karate e la sera passo da Galadriel in ospedale." — ≈4
28. "Matteo ha karate lunedì e giovedì, breakdance il mercoledì e Kodland il venerdì: ogni pomeriggio ha qualcosa." — ≈3
29. "La Kamira fa il caffè, Willie pulisce i pavimenti e il Ficus Grande vuole acqua a parte." — ≈3
30. "Ricordami di fare gli auguri a mia mamma per la festa della mamma il 10 maggio." — ≈1
31. "La telecamera del cancello fa le bizze: manda le foto ma non i video." — ≈2
32. "Nonna Elena dice che il robot aspirapolvere vecchio non funziona più bene, e mia mamma adora i cavalli." — ≈2

## Galadriel (`MWE_ACT_AS=galadriel`) — single facts

33. "Sono Galadriel, la compagna di Frodo."
34. "Tengo un blog di recensioni di romanzi fantasy."
35. "Amo leggere."
36. "Sono celiaca, non posso mangiare glutine."
37. "Sono intollerante al lattosio."
38. "Prendo il Gaviscon mezz'ora dopo pranzo, senza acqua."
39. "Sono incinta."
40. "Sono al quinto mese di gravidanza."
41. "Il 22 giugno devo chiamare il CUP di Comacchio per confermare la prenotazione."
42. "A Matteo non piace il pesto."
43. "Matteo adora andare al McDonald's per incontrare gli amici."
44. "Frodo non sa usare la lavatrice, gli servono le istruzioni."
45. "Ho attivato la condivisione della mia posizione in tempo reale."
46. "Il mio stipendio arriva con i buoni pasto, il buono celiachia e l'assegno unico."

## Galadriel — multi-fact / run-ons

47. "Sono celiaca e intollerante al lattosio, quindi niente glutine e niente latticini per me." — ≈2
48. "Stasera quando passi portami dei cotton fioc, un cuscinetto, un asciugamano intimo e dello yogurt senza lattosio." — ≈4
49. "Per la visita in ospedale mi servono frutta secca, yogurt e degli asciugamani intimi." — ≈3
50. "Sono al quinto mese, la bambina sta bene ed è perfetta, ma ho la pressione un po' alta e forse mi ricoverano." — ≈3
51. "Oggi ho fatto la spesa — latte, formaggio, salame e pane — poi ho portato Matteo a karate e di ritorno sono passata in farmacia per il Gaviscon." — ≈6
52. "Tra le spese fisse abbiamo il teatro, la danza e il karate di Matteo, più Netflix, Prime e Disney+." — ≈6
53. "Domani Frodo deve chiamarmi appena si sveglia per farmi partire la lavatrice, e se passa la sera che mi porti un cuscinetto per la pancia." — ≈2
54. "Il 7 maggio sono andata in ospedale per un controllo e il 9 mi hanno tenuta perché la piccola aveva fretta di uscire." — ≈2
55. "Per la festa della mamma vorrei solo una cena tranquilla a casa, e Frodo deve ricordarsi di chiamare sua madre." — ≈2

## Gollum / Matteo (`MWE_ACT_AS=gollum`) — kid, short

56. "Ho otto anni."
57. "Faccio karate il lunedì e il giovedì."
58. "Il mercoledì faccio breakdance."
59. "Il venerdì ho la lezione online su Kodland alle cinque."
60. "Mamma, voglio andare al McDonald's!"
61. "Non mi piace il pesto."
62. "Sono andato in gita al museo dei dinosauri a Gubbio."
63. "Ho perso il primo dentino!"
64. "Non riesco a dormire, mi canti la ninna nanna?"
65. "Mi fa ancora un po' male dopo l'operazione."
66. "Oggi al karate ho imparato una mossa nuova."
67. "Posso stare sveglio fino a tardi che domani non c'è scuola?"
68. "Voglio guardare i cartoni su Netflix."
69. "A scuola oggi abbiamo studiato i dinosauri."
70. "Il sabato mi piace dormire fino a tardi."

## Bilbo / nonno Bruno (`MWE_ACT_AS=bilbo`)

71. "Sono Bilbo, il padre di Frodo."
72. "Sono il nonno di Matteo."
73. "Sono in ospedale da più di un mese."
74. "Ho avuto delle complicazioni dopo l'operazione."
75. "Mia sorella Adriana viene spesso a trovarmi."
76. "Elena passa molto tempo con me qui in ospedale." — ≈1
77. "Quando posso vedere il piccolo Matteo?"
78. "Adriana oggi mi ha portato il giornale e un po' di frutta." — ≈2

## Group / ambient device-channel (`MWE_ACT_AS=` a family member; collective entities)

These land on shared family pages; the shopping ones pile onto one topic on purpose.

79. "Aggiungete il latte alla lista della spesa."
80. "Manca il detersivo, mettetelo in lista."
81. "Finito lo yogurt senza lattosio, ricompratelo."
82. "Serve il caffè per la Kamira, mettetelo nella spesa."
83. "Comprate mele e banane." — ≈2
84. "Finita la pasta, segnatela sulla lista."
85. "Mettete in lista cotton fioc e asciugamani." — ≈2
86. "Servono pannolini e salviette, aggiungeteli." — ≈2
87. "Stasera si mangia alle otto, come sempre."
88. "Ricordatevi di annaffiare il Ficus Grande."
89. "Matteo deve andare a breakdance, è ora."
90. "Sabato pranzo tutti insieme dai nonni." — ≈1

## Recall — questions, no capture (`intent: recall`)

91. "Quando è nato Matteo?"
92. "Cosa sai del mio lavoro?"
93. "Che giorni fa karate Matteo?"
94. "Qual è la password del WiFi di casa?"
95. "A che ora ceniamo di solito?"
96. "Che intolleranze ha Galadriel?"
97. "Dove lavora Frodo?"
98. "Mi ricordi gli orari di lavoro di Frodo?"

## Structural — reshape a container (`intent: structural`)

99. "Voglio un quaderno per le ricette di famiglia."
100. "Crea una wiki per i viaggi."
101. "Fammi una sezione dedicata alla scuola di Matteo."
102. "Sposta le cose del lavoro in uno spazio a parte."

## Skip — chit-chat, no signal (`intent: skip`)

103. "Ciao Sam!"
104. "Grazie mille, sei un grande."
105. "Buongiorno!"
106. "Haha che forte."
107. "Ok, perfetto."
108. "A dopo!"

---

> 108 lines as written; add more freely below. Keep the `[as]` grouping so each
> goes in with the right act-as, and pile shopping/commission items on purpose —
> the more facts converge on the "lista spesa" topic, the better the emergence
> test downstream.
