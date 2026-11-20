# portable-pty local patch

This copy is based on portable-pty 0.9.0, from WezTerm commit
`f8921727a11b9f8b073e8c24821d72fd41283500`. ConPTY starts without cursor
inheritance so closing a surface cannot wait on its initial cursor query.
