# 関数型コア: Selection は Document の外に置く

編集コアは関数型である: プリミティブは状態を変更するのではなく変換する — `(document, selection) → (new_document, new_selection)`。Document は自身のテキストを所有し、アクティブな Selection は Document に保存されず、各操作に渡される。これは Helix が Selection を View ごとに保持する決定 (1 つのドキュメントを複数のスプリットで表示できる) を踏襲している。初日から採用することで、View が登場したときの API 書き直しを回避できる。
