Target Tokens Cross-Entropy Loss

Main Model

(Next Token Prediction)

Output Head

ℒ𝑀𝑀𝑀𝑀𝑀𝑀𝑀𝑀

Shared

Transformer Block ×𝐿𝐿 Transformer Block ×𝐿𝐿

Transformer Block ×𝐿𝐿

Transformer Block ×𝐿𝐿 Transformer Block ×𝐿𝐿

Embedding Layer nput Tokens Cross-Entropy Loss MTP Module 1

(Next 2 Token Prediction)

Output Head

ℒMTP 1

Shared

Transformer Block

Linear Projection concatenation

RMSNorm RMSNorm

Shared Embedding Layer Cross-Entropy Loss MTP Module 2

(Next 3 Token Prediction)

Output Head

Transformer Block

Linear Projection concatenation

RMSNorm RMSNorm

Shared Embedding Layer

ℒMTP 2

··
