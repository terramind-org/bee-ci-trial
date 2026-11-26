# Feature-to-test mappings

One CSV per task (`<task>.csv`, columns `feature_id,test`). A row says that the named test proves
the feature with that id in `../features.csv`. `scripts/check-coverage.py` merges every file here;
with `--require-tests` it fails for any feature no file maps. Test forms: a cargo test path
(`bee_merge::tests::add_add`), `web:e2e/<file>.spec.ts::<name>`, or `harness:<scenario id>`.
