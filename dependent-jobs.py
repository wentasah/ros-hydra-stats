#!/usr/bin/env python3

import json
import sys

with open(sys.argv[1]) as f:
    jobs = json.load(f)

with open(sys.argv[2]) as f:
    build = json.load(f)

dependent = set()
dependent.add(build['drvpath'])

dep_counts = []
while True:
    for job in jobs:
        if 'inputDrvs' not in job:
            continue
        for input_drv in job['inputDrvs'].keys():
            if input_drv in dependent:
                dependent.add(job['drvPath'])
    if dep_counts and dep_counts[-1] == len(dependent):
        break
    dep_counts.append(len(dependent))

json.dump(dep_counts, sys.stdout)
