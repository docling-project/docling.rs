Target Tokens

𝑡𝑡2

𝑡𝑡3

𝑡𝑡4

𝑡𝑡5

Cross-Entropy Loss

Main Model

(Next Token Prediction)

Output Head

ℒ𝑀𝑀𝑀𝑀𝑀𝑀𝑀𝑀

Shared

Transformer Block ×𝐿𝐿

Transformer Block ×𝐿𝐿

Transformer Block ×𝐿𝐿

Transformer Block ×𝐿𝐿

Transformer Block ×𝐿𝐿

Embedding Layer

nput Tokens

𝑡𝑡

𝑡𝑡

𝑡𝑡

𝑡𝑡

𝑡𝑡3

𝑡𝑡4

𝑡𝑡5

𝑡𝑡6

Cross-Entropy Loss

MTP Module 1

(Next 2 Token Prediction)

Output Head

1

ℒMTP

Shared

Transformer Block

Linear Projection

concatenation

RMSNorm RMSNorm

Shared Embedding Layer

𝑡𝑡

𝑡𝑡

𝑡𝑡

𝑡𝑡

𝑡𝑡4

𝑡𝑡5

𝑡𝑡6

𝑡𝑡7

Cross-Entropy Loss

ℒMTP

MTP Module 2

(Next 3 Token Prediction)

Output Head

Transformer Block

Linear Projection

concatenation

RMSNorm RMSNorm

2

Shared Embedding Layer

··
