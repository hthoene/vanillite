# Runs on every (re)load.
scoreboard objectives add gs dummy
scoreboard players add loads gs 1
data modify storage t:p loads set from storage t:p loads_
execute store result storage t:p loads int 1 run scoreboard players get loads gs
