

```trickle
define operator bucket from grouper::bucket;

define script categorize
script
  let $rate = 1;
  let $dimensions = event.logger_name;
  let $class = "test";
  event
end;

create operator bucket;
create script categorize;

select event from in into categorize;
select event from categorize into bucket;
select event from bucket into out;
```
