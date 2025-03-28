.DEFAULT_GOAL = failed-use-count-sorted.json

CURL = curl -H 'Accept: application/json'
HYDRA = https://hydra.iid.ciirc.cvut.cz

EVAL =

ifneq ($(EVAL),)
eval.json:
	$(CURL) $(HYDRA)/eval/$(EVAL) > $@
else
evals.json:
	$(CURL) $(HYDRA)/jobset/nix-ros-experiments/wentasah-test/evals > $@

eval.json: evals.json
	jq '.evals[0]' $< > $@
endif

builds: eval.json
	mkdir -p $@.tmp
	jq '.builds[]' $< | rush --eta -r1 "$(CURL) -m10 $(HYDRA)/build/{} -o $@.tmp/{}.json -sS"
	mv $@.tmp $@

nix-ros-overlay: eval.json
	set -x; if [ ! -d $@ ]; then \
		git clone $$(jq -r '.jobsetevalinputs."nix-ros-overlay".uri' $<) $@; \
	else \
		git -C $@ fetch $$(jq -r '.jobsetevalinputs."nix-ros-overlay"|.uri,.revision' $<); \
	fi
	set -x; git -C $@ switch --detach $$(jq -r '.jobsetevalinputs."nix-ros-overlay".revision' $<)

jobs.jsonl: nix-ros-overlay
	nix-eval-jobs --expr '(import ./$< {}).rosPackages' > $@.tmp
	mv $@.tmp $@

jobs.json: jobs.jsonl
	jq -s . $< > $@

failed-builds.txt: builds eval.json
	jq '.builds[]|"builds/\(.).json"' eval.json | xargs jq -r 'select(.buildstatus != 0)|.id' > $@

failed-use-count.jsonl: failed-builds.txt jobs.json
	rush --eta -k "jq --slurpfile build builds/{}.json -cs \
	--argjson dep_counts \"\$$(./dependent-jobs.py jobs.json builds/{}.json)\" \
	'\$$build[0] as \$$b|{job: \$$b.job, drv: \$$b.drvpath, id: \$$b.id, count: \$$dep_counts[0], rec_count: \$$dep_counts|last}' jobs.jsonl" \
	< failed-builds.txt > $@

failed-use-count-sorted.json: failed-use-count.jsonl
	jq -cs 'sort_by(.rec_count)|.[]|{job,count,rec_count,url: "$(HYDRA)/build/\(.id)"}' $<
