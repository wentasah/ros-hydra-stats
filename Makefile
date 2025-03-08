.DEFAULT_GOAL = failed-use-count-sorted.json

CURL = curl -H 'Accept: application/json'
HYDRA = https://hydra.iid.ciirc.cvut.cz

EVAL =

#ifneq ($(EVAL),)
eval.json:
	$(CURL) $(HYDRA)/eval/$(EVAL) > $@
# else
# evals.json:
# 	$(CURL) $(HYDRA)/jobset/nix-ros-experiments/wentasah-test/evals > $@

# eval.json: evals.json
# 	jq '.evals[0]' $< > $@
# endif

builds: eval.json
	mkdir -p $@.tmp
	jq '.builds[]' $< | rush --eta -r1 "$(CURL) -m10 $(HYDRA)/build/{} -o $@.tmp/{}.json -sS"
	mv $@.tmp $@

nix-ros-overlay: eval.json
	set -x; git clone $$(jq -r '.jobsetevalinputs."nix-ros-overlay".uri' $<) $@
	set -x; git -C $@ switch --detach $$(jq -r '.jobsetevalinputs."nix-ros-overlay".revision' $<)

jobs.jsonl: nix-ros-overlay
	nix-eval-jobs --expr '(import ./$< {}).rosPackages' > $@.tmp
	mv $@.tmp $@

failed-builds.txt: builds
	jq -r 'select(.buildstatus != 0)|.id' $</*.json > $@

failed-use-count.jsonl: failed-builds.txt jobs.jsonl
	rush --eta -k "jq --slurpfile build builds/{}.json -cs '\$$build[0] as \$$b|{job: \$$b.job, drv: \$$b.drvpath, id: \$$b.id, count: [.[]|select(.inputDrvs|has(\$$b.drvpath)).attr]|length}' jobs.jsonl" < failed-builds.txt > $@

failed-use-count-sorted.json: failed-use-count.jsonl
	jq -cs 'sort_by(.count)|.[]|{job,count,url: "$(HYDRA)/build/\(.id)"}' $<
