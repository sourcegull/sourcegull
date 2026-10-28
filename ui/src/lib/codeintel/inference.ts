/**
 * The site admin's auto-indexing inference script: read, save, and try it on a
 * repository.
 */

export const INFERENCE_SCRIPT = `query CodeIntelligenceInferenceScript { codeIntelligenceInferenceScript }`;

export const UPDATE_INFERENCE_SCRIPT = `mutation UpdateCodeIntelligenceInferenceScript($script: String!) {
	updateCodeIntelligenceInferenceScript(script: $script) { alwaysNil }
}`;

export const REPOSITORY_ID = `query InferenceRepositoryId($name: String!) { repository(name: $name) { id } }`;

export const INFER_AUTO_INDEX_JOBS = `query InferAutoIndexJobsForRepo($repo: ID!, $script: String) {
	inferAutoIndexJobsForRepo(repository: $repo, script: $script) {
		jobs { root indexer { name imageName } steps { index { indexerArgs } } }
		inferenceOutput
	}
}`;

export interface InferredJob {
	root: string;
	indexer: { name: string; imageName: string | null } | null;
	steps: { index: { indexerArgs: string[] } };
}

/** A starting point for a script. */
export const EXAMPLE_SCRIPT = `-- Return recognizers to add, or set a built-in one (sg.go, sg.java, sg.python, sg.ruby,
-- sg.rust, sg.typescript, sg.dotnet, sg.clang) to false to disable it.
local path = require("path")
local pattern = require("sg.autoindex.patterns")
local recognizer = require("sg.autoindex.recognizer")

return require("sg.autoindex.config").new({
  ["acme.custom"] = recognizer.new_path_recognizer({
    patterns = { pattern.new_path_basename("acme.yaml") },
    generate = function(_, paths)
      local jobs = {}
      for _, p in ipairs(paths) do
        table.insert(jobs, {
          root = path.dirname(p),
          indexer = "registry.example.com/acme-indexer:1",
          indexer_args = { "acme-index" },
          outfile = "index.scip",
        })
      end
      return jobs
    end,
  }),
})
`;
